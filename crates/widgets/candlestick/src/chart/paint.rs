//! The canvas that draws one candle chart.
//!
//! Drawing happens in two passes, and the split is what makes panning and
//! hovering cheap:
//!
//! - **prepaint** resolves the layout, which candles are visible, every label
//!   the axes will show, and the pixel box of every candle body, wick and volume
//!   bar. All of it is cached against the exact inputs that produced it, so a
//!   frame that only moved the pointer re-uses the previous result instead of
//!   re-deriving hundreds of boxes and shaping dozens of labels.
//! - **paint** turns those boxes into quads, plus the crosshair, which is the
//!   only part that must follow the pointer every frame.

use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

use base_ui::style::theme::{Theme, ThemeExt};
use gpui_kit::{
    App, Bounds, ContentMask, Hsla, IntoElement, Path, PathBuilder, Pixels, Point, ShapedLine,
    SharedString, Styled, TextAlign, TextStyle, Window, canvas, fill, point, px, size,
};
use i18n::t;
use market_participants_connector::{
    connector::MarketParticipantsConnector, market::timeframe::TimeFrame,
};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Quantity, Tick},
    market::{book::ListingCandlestick, units::QuantityUnit},
};
use smol_str::{SmolStr, format_smolstr};
use time::UtcOffset;
use widget::time_zone::data_time_zone;

use super::{
    geometry::{
        CandleGeometry, ChartLayout, ChartViewport, LayoutInputs, SubPlotLayout, visible_range,
    },
    text::{
        ChartTextCache, HOVER_LABEL_LINE_HEIGHT, HoverLabelCache, format_price,
        format_signed_price, format_volume,
    },
};
use crate::{
    indicator::{
        CandlestickIndicatorConfig, IndicatorId, IndicatorInput, IndicatorPane, IndicatorSeries,
        LineShape, ValueRange, compute_indicators, definition_of, enabled_sub_plot_ids,
    },
    model::{CandlePrices, CandlestickSeriesSnapshot},
    time::{format_axis_time, maximum_countdown_label},
};

/// Distance a label is inset from an axis's own edge.
const AXIS_TICK_TEXT_INSET: f32 = 10.0;
const AXIS_TAG_TEXT_INSET: f32 = 8.0;
/// Height of the tag carrying the latest price and the countdown.
const LATEST_PRICE_TAG_HEIGHT: f32 = 38.0;
const LATEST_PRICE_TAG_PRICE_HEIGHT: f32 = 20.0;
/// How far the chart may be scrolled away from the live edge before the latest
/// price indicator stops being drawn.
const LATEST_PRICE_MAX_CANDLE_GAP: usize = 40;
const PRICE_AXIS_TARGET_LABEL_SPACING: f32 = 44.0;
const PRICE_AXIS_MIN_LABEL_SPACING: f32 = 28.0;
const MAX_PRICE_AXIS_TARGET_INTERVALS: f64 = 16.0;
const MAX_PRICE_AXIS_TICKS: usize = 24;
const HOVER_LABEL_INSET: f32 = 4.0;
/// The gap between the main-chart legend row and the OHLC hover label below it.
///
/// Both used to start at [`HOVER_LABEL_INSET`] from the plot's top-left corner,
/// so the hover label was painted on top of the legend. The legend's position is
/// fixed by the design (top-left, inset by `HOVER_LABEL_INSET`); the hover label
/// is not, so it moves down one line while the legend is shown.
const HOVER_LABEL_LEGEND_GAP: f32 = 2.0;
const VOLUME_HEADROOM: f64 = 1.08;
/// Below five label rows a sub plot labels three values instead of five.
///
/// Five labels need five [`HOVER_LABEL_LINE_HEIGHT`] rows; a shorter plot would
/// draw the rows on top of each other.
const SUB_PLOT_COMPACT_HEIGHT: f32 = 5.0 * HOVER_LABEL_LINE_HEIGHT;

/// Everything one frame of the chart draws from.
pub(crate) struct ChartCanvasOptions {
    pub viewport: ChartViewport,
    pub interval: TimeFrame,
    pub countdown_label: SharedString,
    /// The indicator configuration. It keys both the value cache and the
    /// prepared chart, because it decides the values and the layout.
    pub indicator_config: CandlestickIndicatorConfig,
    /// Whether the settings panel is open, which decides the chart's width.
    pub panel_open: bool,
}

/// Per-chart caches that survive between frames.
///
/// They live in the view rather than in the element because the element is
/// rebuilt every frame, and are cleared whenever the data underneath them is
/// replaced.
#[derive(Default)]
pub(crate) struct ChartPaintCaches {
    text: RefCell<ChartTextCache>,
    prepared: RefCell<PreparedChartCache>,
    indicators: RefCell<IndicatorCache>,
}

impl ChartPaintCaches {
    pub(crate) fn clear(&self) {
        self.text.borrow_mut().clear();
        *self.prepared.borrow_mut() = PreparedChartCache::default();
        *self.indicators.borrow_mut() = IndicatorCache::default();
    }
}

/// The chart element.
///
/// The boxes are resolved from the bounds the element is given, and both
/// the first layout and every later change of width are reported through
/// `on_layout_change`: how many candles fit is a function of the plot width, so
/// whoever owns the data has to hear about a pane that grew.
pub(crate) fn chart_canvas(
    snapshot: CandlestickSeriesSnapshot,
    options: ChartCanvasOptions,
    layout_cell: Rc<RefCell<Option<Rc<ChartLayout>>>>,
    hover_label_cache: Rc<RefCell<HoverLabelCache>>,
    paint_caches: Rc<ChartPaintCaches>,
    on_layout_change: impl FnOnce(&mut App) + 'static,
) -> impl IntoElement {
    let prepaint_snapshot = snapshot.clone();
    let prepaint_viewport = options.viewport;
    let prepaint_interval = options.interval;
    let prepaint_indicator_config = options.indicator_config.clone();
    let prepaint_panel_open = options.panel_open;
    let paint_hover_label_cache = hover_label_cache;
    let prepaint_caches = paint_caches.clone();
    let paint_caches = paint_caches;
    canvas(
        move |bounds, window, cx| {
            let prepared = prepare_chart_cached(
                &prepaint_caches,
                &prepaint_snapshot,
                prepaint_viewport,
                prepaint_interval,
                data_time_zone(cx),
                bounds,
                cx.theme().components.chart.axis.hsla,
                &prepaint_indicator_config,
                prepaint_panel_open,
                window,
            );
            // The view hit-tests pointer positions against this, and only the
            // layout pass knows the element's bounds. It is set before the change
            // is reported, because what reacts to it reads the new boxes back.
            let previous = layout_cell.borrow_mut().replace(prepared.layout.clone());
            if previous.is_none_or(|previous| {
                previous.maximum_plot_width() != prepared.layout.maximum_plot_width()
            }) {
                on_layout_change(cx);
            }
            prepared
        },
        move |_, prepared, window, context| {
            paint_chart(
                &snapshot,
                &options,
                prepared.as_ref(),
                &paint_hover_label_cache,
                &paint_caches.text,
                window,
                context,
            );
        },
    )
    .size_full()
}

struct PreparedChart {
    layout: Rc<ChartLayout>,
    visible: Option<PreparedVisibleChart>,
    indicators: Arc<[IndicatorSeries]>,
}

#[derive(Clone)]
struct PreparedAxisTick {
    value: f64,
    label: SharedString,
}

#[derive(Clone)]
struct PreparedTimeTick {
    x: Pixels,
    label: SharedString,
}

struct PreparedLatestPrice {
    y: Pixels,
    label: SharedString,
    is_up: bool,
}

struct PreparedCandle {
    wick: Bounds<Pixels>,
    body: Bounds<Pixels>,
    volume: Option<Bounds<Pixels>>,
    is_up: bool,
}

struct PreparedVisibleChart {
    range: Range<usize>,
    lowest: f64,
    highest: f64,
    geometry: CandleGeometry,
    candles: Arc<[PreparedCandle]>,
    price_ticks: Arc<[PreparedAxisTick]>,
    time_ticks: Arc<[PreparedTimeTick]>,
    latest_price: Option<PreparedLatestPrice>,
    /// One entry per sub plot, aligned with [`ChartLayout::sub_plots`].
    sub_plots: Arc<[PreparedSubPlot]>,
}

/// One sub plot's identity and visible range.
///
/// The axis ticks and labels are derived at paint time from the range and the
/// plot height, so they are not prepared here; this is what the pointer needs to
/// answer which indicator a sub plot shows and what value a y coordinate means.
struct PreparedSubPlot {
    id: IndicatorId,
    value_range: ValueRange,
}

/// The inputs the prepared chart was built from.
struct PreparedChartKey {
    snapshot: CandlestickSeriesSnapshot,
    candle_width: f32,
    right_timestamp: Option<i64>,
    interval: TimeFrame,
    /// The zone the time axis was formatted in.
    zone: UtcOffset,
    bounds: Bounds<Pixels>,
    text_style: TextStyle,
    text_color: Hsla,
    /// Decides the layout (how many sub plots) and which lines are drawn.
    indicator_config: CandlestickIndicatorConfig,
    /// Decides whether the chart is narrower than the whole body.
    panel_open: bool,
}

impl PreparedChartKey {
    fn matches(
        &self,
        snapshot: &CandlestickSeriesSnapshot,
        viewport: ChartViewport,
        interval: TimeFrame,
        zone: UtcOffset,
        bounds: Bounds<Pixels>,
        text_style: &TextStyle,
        text_color: Hsla,
        indicator_config: &CandlestickIndicatorConfig,
        panel_open: bool,
    ) -> bool {
        self.snapshot.shares_chart_data_with(snapshot)
            && self.candle_width == viewport.candle_width
            && self.right_timestamp == viewport.right_timestamp
            && self.interval == interval
            && self.zone == zone
            && self.bounds == bounds
            && self.text_style == *text_style
            && self.text_color == text_color
            && self.indicator_config == *indicator_config
            && self.panel_open == panel_open
    }
}

#[derive(Default)]
struct PreparedChartCache {
    key: Option<PreparedChartKey>,
    chart: Option<Rc<PreparedChart>>,
    /// Counts real rebuilds, so a test can tell a cache hit from a redraw.
    #[cfg(test)]
    builds: usize,
}

/// The computed indicator values, keyed only by the snapshot and the
/// configuration.
///
/// Values do not depend on the viewport, so panning and zooming must not rebuild
/// them: that is why this is a cache of its own rather than a field of
/// [`PreparedChart`], whose key changes on every pan.
#[derive(Default)]
struct IndicatorCache {
    snapshot: Option<CandlestickSeriesSnapshot>,
    config: Option<CandlestickIndicatorConfig>,
    series: Arc<[IndicatorSeries]>,
    /// Counts real rebuilds, so a test can tell a cache hit from a recompute.
    #[cfg(test)]
    builds: usize,
}

impl IndicatorCache {
    /// Whether the cache holds the values for this snapshot and configuration.
    ///
    /// The snapshot is compared with [`CandlestickSeriesSnapshot::shares_chart_data_with`]
    /// (an `Arc` pointer comparison), not with `==`: snapshots deliberately have
    /// no `PartialEq`, which would compare every candle.
    fn matches(
        &self,
        snapshot: &CandlestickSeriesSnapshot,
        config: &CandlestickIndicatorConfig,
    ) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|cached| cached.shares_chart_data_with(snapshot))
            && self.config.as_ref() == Some(config)
    }
}

/// The computed series for `snapshot` and `config`, from the cache when possible.
///
/// The input columns are built **only** on a miss: allocating them on every frame
/// would be the per-frame cost this cache exists to avoid.
fn indicator_series(
    caches: &ChartPaintCaches,
    snapshot: &CandlestickSeriesSnapshot,
    config: &CandlestickIndicatorConfig,
) -> Arc<[IndicatorSeries]> {
    {
        let cache = caches.indicators.borrow();
        if cache.matches(snapshot, config) {
            return cache.series.clone();
        }
    }

    let inputs: Vec<IndicatorInput> = snapshot
        .candles()
        .iter()
        .map(|candle| {
            let prices = CandlePrices::of(candle);
            IndicatorInput {
                close: prices.close().inner() as f64,
                high: prices.high().inner() as f64,
                low: prices.low().inner() as f64,
            }
        })
        .collect();
    let series: Arc<[IndicatorSeries]> = compute_indicators(&inputs, config).into();

    let mut cache = caches.indicators.borrow_mut();
    cache.snapshot = Some(snapshot.clone());
    cache.config = Some(config.clone());
    cache.series = series.clone();
    #[cfg(test)]
    {
        cache.builds += 1;
    }
    series
}

fn prepare_chart_cached(
    caches: &ChartPaintCaches,
    snapshot: &CandlestickSeriesSnapshot,
    viewport: ChartViewport,
    interval: TimeFrame,
    zone: UtcOffset,
    bounds: Bounds<Pixels>,
    text_color: Hsla,
    indicator_config: &CandlestickIndicatorConfig,
    panel_open: bool,
    window: &Window,
) -> Rc<PreparedChart> {
    let text_style = window.text_style();
    let cached = {
        let cache = caches.prepared.borrow();
        cache.key.as_ref().and_then(|key| {
            if key.matches(
                snapshot,
                viewport,
                interval,
                zone,
                bounds,
                &text_style,
                text_color,
                indicator_config,
                panel_open,
            ) {
                cache.chart.clone()
            } else {
                None
            }
        })
    };
    if let Some(chart) = cached {
        return chart;
    }

    // The sub-plot count and their order come from the configuration, not from
    // the computed series: VOL is a sub plot with no computed values.
    let sub_plot_ids = enabled_sub_plot_ids(indicator_config);
    // The values depend only on the snapshot and the configuration, so they are
    // computed once and then only re-read by the layout loop and the painter.
    let indicators = indicator_series(caches, snapshot, indicator_config);
    let layout = resolve_chart_layout(
        snapshot,
        viewport,
        interval,
        bounds,
        text_color,
        &indicators,
        &sub_plot_ids,
        &caches.text,
        window,
    );
    let prepared = Rc::new(prepare_visible_chart(
        snapshot,
        viewport,
        interval,
        zone,
        layout,
        indicators,
        &sub_plot_ids,
    ));
    let mut cache = caches.prepared.borrow_mut();
    cache.key = Some(PreparedChartKey {
        snapshot: snapshot.clone(),
        candle_width: viewport.candle_width,
        right_timestamp: viewport.right_timestamp,
        interval,
        zone,
        bounds,
        text_style,
        text_color,
        indicator_config: indicator_config.clone(),
        panel_open,
    });
    cache.chart = Some(prepared.clone());
    #[cfg(test)]
    {
        cache.builds += 1;
    }
    prepared
}

fn prepare_visible_chart(
    snapshot: &CandlestickSeriesSnapshot,
    viewport: ChartViewport,
    interval: TimeFrame,
    zone: UtcOffset,
    layout: ChartLayout,
    indicators: Arc<[IndicatorSeries]>,
    sub_plot_ids: &[IndicatorId],
) -> PreparedChart {
    let range = visible_range(snapshot.candles(), viewport, layout.price_plot.size.width);
    // The volume bars live in the VOL sub plot, and only there. When VOL is not
    // the first enabled sub plot there is no box for them, so no bar is prepared
    // and the painter draws none.
    let volume_plot = if sub_plot_ids.first() == Some(&IndicatorId::Vol) {
        layout.sub_plots.first().map(|sub_plot| sub_plot.plot)
    } else {
        None
    };
    let visible = (!range.is_empty()).then(|| {
        let candles = snapshot.candles();
        let latest = latest_candle_for_indicator(snapshot, range.end);
        let visible = &candles[range.clone()];
        let (lowest, highest) = price_bounds_with_indicators(
            candles,
            range.clone(),
            latest.map(|candle| CandlePrices::of(candle).close()),
            &indicators,
        );
        let volume_upper = volume_upper_bound(visible);
        let sub_plots: Arc<[PreparedSubPlot]> = if layout.sub_plots.is_empty() {
            Vec::new().into()
        } else {
            debug_assert_eq!(
                sub_plot_ids.len(),
                layout.sub_plots.len(),
                "the layout draws one box per enabled sub plot"
            );
            sub_plot_ids
                .iter()
                .map(|id| PreparedSubPlot {
                    id: *id,
                    value_range: sub_plot_value_range(
                        *id,
                        volume_upper,
                        range.clone(),
                        &indicators,
                    ),
                })
                .collect::<Vec<_>>()
                .into()
        };
        let geometry = CandleGeometry::new(layout.price_plot, range.len(), viewport.candle_width);
        let body_width = geometry.body_width();
        let prepared_candles: Arc<[PreparedCandle]> = visible
            .iter()
            .enumerate()
            .map(|(index, candle)| {
                let prices = CandlePrices::of(candle);
                let center_x = geometry.center_x(index);
                PreparedCandle {
                    wick: candle_wick_bounds(prices, center_x, lowest, highest, layout.price_plot),
                    body: candle_body_bounds(
                        prices,
                        center_x,
                        body_width,
                        lowest,
                        highest,
                        layout.price_plot,
                    ),
                    volume: candle_volume_bounds(
                        candle,
                        center_x,
                        body_width,
                        volume_upper,
                        volume_plot,
                    ),
                    is_up: prices.is_up(),
                }
            })
            .collect::<Vec<_>>()
            .into();
        let price_ticks = price_axis_values(
            lowest,
            highest,
            layout.price_plot.size.height,
            snapshot.price_tick(),
        )
        .into_iter()
        .map(|value| PreparedAxisTick {
            value,
            label: SharedString::new(format_price(value, snapshot.price_tick()).as_str()),
        })
        .collect::<Vec<_>>();
        // One label every 70px at most, aligned to a multiple of the stride so
        // that panning does not make the labels crawl.
        let label_stride = ((70.0 / viewport.candle_width).ceil() as usize).max(1);
        let first_tick = range.start.div_ceil(label_stride) * label_stride;
        let time_ticks = (first_tick..range.end)
            .step_by(label_stride)
            .map(|index| PreparedTimeTick {
                x: geometry.center_x(index - range.start),
                label: SharedString::new(
                    format_axis_time(candles[index].timestamp, interval, zone).as_str(),
                ),
            })
            .collect::<Vec<_>>();
        let latest_price = latest.map(|candle| {
            let prices = CandlePrices::of(candle);
            PreparedLatestPrice {
                y: y_for_price(
                    prices.close().inner() as f64,
                    lowest,
                    highest,
                    layout.price_plot,
                ),
                label: SharedString::new(
                    prices
                        .close()
                        .format_and_round_tick(snapshot.price_tick())
                        .as_str(),
                ),
                is_up: prices.is_up(),
            }
        });
        PreparedVisibleChart {
            range,
            lowest,
            highest,
            geometry,
            candles: prepared_candles,
            price_ticks: price_ticks.into(),
            time_ticks: time_ticks.into(),
            latest_price,
            sub_plots,
        }
    });
    PreparedChart {
        layout: Rc::new(layout),
        visible,
        indicators,
    }
}

/// Resolves the layout, widening the y-axes until they fit their own labels.
fn resolve_chart_layout(
    snapshot: &CandlestickSeriesSnapshot,
    viewport: ChartViewport,
    interval: TimeFrame,
    bounds: Bounds<Pixels>,
    text_color: Hsla,
    main_series: &[IndicatorSeries],
    sub_plot_ids: &[IndicatorId],
    text_cache: &RefCell<ChartTextCache>,
    window: &Window,
) -> ChartLayout {
    let mut longest_label_width = px(0.0);
    // The number of sub plots is the number of enabled sub-plot indicators, in
    // catalogue order. VOL is one of them even though it has no computed series.
    let mut layout = ChartLayout::new(
        bounds,
        LayoutInputs {
            longest_label_width,
            sub_plot_count: sub_plot_ids.len(),
        },
    );
    let text_measurer = AxisTextMeasurer {
        color: text_color,
        cache: text_cache,
        window,
    };
    // A wider axis only removes candles from the left, so retaining the widest
    // result converges.
    for _ in 0..3 {
        let range = visible_range(snapshot.candles(), viewport, layout.price_plot.size.width);
        let latest = latest_candle_for_indicator(snapshot, range.end);
        let measured_width = widest_axis_label_width(
            snapshot.candles(),
            range,
            latest,
            snapshot.price_tick(),
            layout.price_plot.size.height,
            interval,
            main_series,
            sub_plot_ids,
            &text_measurer,
        );
        longest_label_width = longest_label_width.max(measured_width);
        let next = ChartLayout::new(
            bounds,
            LayoutInputs {
                longest_label_width,
                sub_plot_count: sub_plot_ids.len(),
            },
        );
        if next.price_axis.size.width <= layout.price_axis.size.width {
            break;
        }
        layout = next;
    }
    layout
}

fn widest_axis_label_width(
    candles: &[ListingCandlestick],
    range: Range<usize>,
    latest: Option<&ListingCandlestick>,
    price_tick: Tick,
    price_plot_height: Pixels,
    interval: TimeFrame,
    main_series: &[IndicatorSeries],
    sub_plot_ids: &[IndicatorId],
    text_measurer: &AxisTextMeasurer<'_>,
) -> Pixels {
    let widest_axis_label = axis_label_candidates(
        candles,
        range,
        latest.map(|candle| CandlePrices::of(candle).close()),
        price_tick,
        price_plot_height,
        main_series,
        sub_plot_ids,
    )
    .into_iter()
    .map(|label| text_measurer.measure(label))
    .fold(px(0.0), Pixels::max);
    let countdown_width = latest.map_or(px(0.0), |_| {
        text_measurer.measure(maximum_countdown_label(interval))
    });
    widest_axis_label.max(countdown_width)
}

/// Every label the axes might draw, including the ones rounding could produce.
///
/// The axis is sized once per frame from this list, so it must be a superset: a
/// label that appears later must not widen the axis, because that would take
/// width away from the plot and change the very candles it was measured from.
/// That is also why the price labels are derived from the same
/// [`price_bounds_with_indicators`] the plot uses: a main-chart line that moved
/// the bound must move the candidate set with it.
///
/// The sub plots contribute their labels too: all y-axes share one width, so a
/// signed MACD tick must be able to widen the price axis as well (design §6.1).
fn axis_label_candidates(
    candles: &[ListingCandlestick],
    range: Range<usize>,
    latest_price: Option<Price>,
    price_tick: Tick,
    price_plot_height: Pixels,
    main_series: &[IndicatorSeries],
    sub_plot_ids: &[IndicatorId],
) -> Vec<SmolStr> {
    if range.is_empty() {
        return vec![SmolStr::new_static("0.000")];
    }

    let visible = &candles[range.clone()];
    let volume_upper = volume_upper_bound(visible);
    let sub_ranges: Vec<(IndicatorId, ValueRange)> = sub_plot_ids
        .iter()
        .map(|id| {
            (
                *id,
                sub_plot_value_range(*id, volume_upper, range.clone(), main_series),
            )
        })
        .collect();
    let (lowest, highest) = price_bounds_with_indicators(candles, range, latest_price, main_series);
    let mut labels = Vec::with_capacity(32);
    labels.extend(
        price_axis_values(lowest, highest, price_plot_height, price_tick)
            .into_iter()
            .map(|value| format_price(value, price_tick)),
    );
    labels.push(format_price(lowest, price_tick));
    labels.push(format_price(highest, price_tick));
    if let Some(latest_price) = latest_price {
        labels.push(latest_price.format_and_round_tick(price_tick));
    }
    for (id, value_range) in &sub_ranges {
        match id {
            IndicatorId::Vol => {
                labels.extend(
                    volume_axis_values(value_range.highest)
                        .into_iter()
                        .map(format_volume),
                );
                labels.push(format_volume(value_range.highest));

                let scale = Quantity::from(1_u64).inner() as f64;
                let below_thousand = scale * 1_000.0 - scale / 1_000.0;
                if value_range.highest >= below_thousand {
                    labels.push(format_volume(below_thousand));
                }
                // Rounding immediately below a unit boundary can add a digit before the suffix changes.
                let rounded_below_thousand = scale * 1_000.0 - scale / 4_000.0;
                if value_range.highest >= rounded_below_thousand {
                    labels.push(format_volume(rounded_below_thousand));
                }
                let below_million = scale * 1_000_000.0 - scale;
                if value_range.highest >= below_million {
                    labels.push(format_volume(below_million));
                }
                let rounded_below_million = scale * 1_000_000.0 - scale / 4.0;
                if value_range.highest >= rounded_below_million {
                    labels.push(format_volume(rounded_below_million));
                }
            }
            IndicatorId::Macd => {
                labels.extend(
                    [value_range.lowest, 0.0, value_range.highest]
                        .into_iter()
                        .map(|value| format_signed_price(value, price_tick)),
                );
            }
            IndicatorId::Rsi => {
                labels.extend([0.0, 30.0, 50.0, 70.0, 100.0].map(format_percentage_tick));
            }
            IndicatorId::Kdj => {
                // Cover both height modes: the compact one labels 0/50/100, the
                // tall one labels the 20/80 guides plus any bound that left the
                // domain. `kdj_axis_values` is the same chooser the painter uses,
                // so the axis is never sized for a label it will not draw.
                labels.extend([0.0, 50.0, 100.0].map(format_percentage_tick));
                labels.extend(
                    kdj_axis_values(*value_range)
                        .into_iter()
                        .map(format_percentage_tick),
                );
            }
            IndicatorId::Ma | IndicatorId::Ema => {}
        }
    }
    labels
}

fn axis_text_width(axis_width: Pixels, inset: f32) -> Pixels {
    (axis_width - px(inset)).max(px(0.0))
}

struct AxisTextMeasurer<'a> {
    color: Hsla,
    cache: &'a RefCell<ChartTextCache>,
    window: &'a Window,
}

impl AxisTextMeasurer<'_> {
    fn measure(&self, text: impl AsRef<str>) -> Pixels {
        self.cache
            .borrow_mut()
            .width(text.as_ref(), self.color, self.window)
    }
}

struct ChartPainter<'a> {
    text_cache: &'a RefCell<ChartTextCache>,
    window: &'a mut Window,
    context: &'a mut App,
    /// The snapshot's price tick, which the MACD axis and legend format with.
    price_tick: Tick,
}

/// Top inset of the OHLC hover label.
///
/// The main-chart legend owns the plot's first text row while it is drawn, so
/// the label starts one line below it (with a small gap). Without a legend it
/// keeps the original top-left inset.
fn hover_label_top(has_legend: bool) -> Pixels {
    if has_legend {
        px(HOVER_LABEL_INSET + HOVER_LABEL_LINE_HEIGHT + HOVER_LABEL_LEGEND_GAP)
    } else {
        px(HOVER_LABEL_INSET)
    }
}

fn paint_chart(
    snapshot: &CandlestickSeriesSnapshot,
    options: &ChartCanvasOptions,
    prepared: &PreparedChart,
    hover_label_cache: &Rc<RefCell<HoverLabelCache>>,
    text_cache: &RefCell<ChartTextCache>,
    window: &mut Window,
    context: &mut App,
) {
    let PreparedChart {
        layout,
        visible,
        indicators,
    } = prepared;
    let layout = layout.as_ref();
    // No background quad: the pane body is the background, and a quad of our own
    // would be a rectangle drawn over its rounded corners.
    let grid = context.theme().components.chart.grid.hsla;
    let data_bounds = layout.data_bounds();
    window.paint_quad(fill(
        Bounds::new(
            point(layout.price_plot.right(), layout.price_plot.origin.y),
            size(px(1.0), data_bounds.size.height),
        ),
        grid,
    ));
    for separator in &layout.separators {
        window.paint_quad(fill(*separator, grid));
    }
    // There is no separator box between the last sub plot and the time axis;
    // this top edge is where that boundary is drawn.
    window.paint_quad(fill(
        Bounds::new(
            point(data_bounds.origin.x, data_bounds.bottom()),
            size(data_bounds.size.width, px(1.0)),
        ),
        grid,
    ));

    let Some(visible) = visible else {
        return;
    };
    // The crosshair's date is read in the zone the settings display data in, and it is
    // read here rather than remembered: a frame after the setting moved paints the new one.
    let zone = data_time_zone(context);
    let price_tick = snapshot.price_tick();
    let mut painter = ChartPainter {
        text_cache,
        window,
        context,
        price_tick,
    };
    painter.paint_price_axis(
        &visible.price_ticks,
        visible.lowest,
        visible.highest,
        layout,
    );
    // The vertical grid is painted before every plot's data, so a line or a bar
    // is drawn on top of it rather than under it.
    painter.paint_time_axis(&visible.time_ticks, layout);
    // A sub plot draws its own axis, guides and data. The boxes and the prepared
    // ranges are aligned by index, and both follow catalogue order.
    for (index, sub_plot) in layout.sub_plots.iter().enumerate() {
        let Some(prepared_sub_plot) = visible.sub_plots.get(index) else {
            continue;
        };
        let series = indicators
            .iter()
            .find(|series| series.id == prepared_sub_plot.id);
        painter.paint_sub_plot(
            prepared_sub_plot.id,
            series,
            *sub_plot,
            visible.range.clone(),
            visible.geometry,
            prepared_sub_plot.value_range,
        );
    }
    // Main-chart indicator lines sit above the grid and below the candles: a
    // line drawn after the wicks would cover them (design §8.1).
    painter.paint_main_indicator_lines(indicators, visible, layout);
    // The candle the crosshair is on, if the pointer is over any plot. Its
    // volume bar is painted at full strength along with it.
    let hovered_candle = options
        .viewport
        .cursor
        .filter(|cursor| layout.contains_data_point(cursor))
        .and_then(|cursor| visible.geometry.slot_at(cursor.x));
    paint_candle_series(
        &visible.candles,
        hovered_candle,
        layout,
        painter.context.theme(),
        &mut *painter.window,
    );
    if let Some(latest) = &visible.latest_price {
        painter.paint_latest_price_indicator(latest, options.countdown_label.clone(), layout);
    }
    // The legend reads the candle under the pointer, or the newest visible one
    // when the pointer is away. The absolute index is what indexes the values;
    // a visible slot alone would read another candle's values after a pan.
    let absolute = legend_candle_index(options.viewport.cursor, layout, visible);
    let legend = absolute
        .map(|absolute| main_indicator_legend(indicators, absolute, price_tick))
        .unwrap_or_default();
    if !legend.is_empty() {
        painter.paint_legend(&legend, layout.price_plot);
    }
    if let Some(absolute) = absolute {
        let axis_color = painter.context.theme().components.chart.axis.hsla;
        for (index, prepared_sub_plot) in visible.sub_plots.iter().enumerate() {
            let Some(sub_plot) = layout.sub_plots.get(index) else {
                continue;
            };
            let entries = sub_plot_legend(
                prepared_sub_plot.id,
                indicators,
                absolute,
                snapshot,
                price_tick,
                axis_color,
            );
            if !entries.is_empty() {
                painter.paint_legend(&entries, sub_plot.plot);
            }
        }
    }
    if let Some(cursor) = options
        .viewport
        .cursor
        .filter(|cursor| layout.contains_data_point(cursor))
    {
        // The legend owns the plot's first text row; the OHLC label takes the
        // next one so the two are both readable.
        let hover_label_top = hover_label_top(!legend.is_empty());
        painter.paint_crosshair(
            snapshot,
            visible,
            options.interval,
            zone,
            cursor,
            layout,
            hover_label_cache,
            hover_label_top,
        );
    }
}

/// The colour of one MACD histogram bar.
///
/// A non-negative bar points up and takes the bullish colour; a negative one
/// points down and takes the bearish colour. Zero counts as positive, so a flat
/// histogram does not flicker between the two colours.
fn histogram_color(value: f64, bullish: Hsla, bearish: Hsla) -> Hsla {
    if value >= 0.0 { bullish } else { bearish }
}

impl ChartPainter<'_> {
    /// Draws every enabled main-chart line.
    ///
    /// The path's `PathBuilder` does not clip, so the drawing is wrapped in a
    /// content mask: a value outside the price bounds can still be produced by
    /// axis rounding, and the line must not run into the axis.
    fn paint_main_indicator_lines(
        &mut self,
        indicators: &[IndicatorSeries],
        visible: &PreparedVisibleChart,
        layout: &ChartLayout,
    ) {
        for series in indicators {
            if definition_of(series.id).pane != IndicatorPane::Main {
                continue;
            }
            for line in &series.lines {
                if line.shape != LineShape::Line {
                    continue;
                }
                let Some(path) = build_indicator_path(
                    &line.values,
                    visible.range.clone(),
                    visible.geometry,
                    |value| y_for_price(value, visible.lowest, visible.highest, layout.price_plot),
                ) else {
                    continue;
                };
                let price_plot = layout.price_plot;
                self.window.paint_layer(price_plot, |window| {
                    window.with_content_mask(Some(ContentMask { bounds: price_plot }), |window| {
                        window.paint_path(path, line.color);
                    });
                });
            }
        }
    }

    /// Draws one legend row in a plot's top-left corner, each entry in its own
    /// colour.
    fn paint_legend(&mut self, entries: &[LegendEntry], plot: Bounds<Pixels>) {
        let mut x = plot.origin.x + px(HOVER_LABEL_INSET);
        let y = plot.origin.y + px(HOVER_LABEL_INSET);
        let ChartPainter {
            text_cache,
            window,
            context,
            ..
        } = self;
        window.with_content_mask(Some(ContentMask { bounds: plot }), |window| {
            for entry in entries {
                if x >= plot.right() {
                    break;
                }
                let line = text_cache.borrow_mut().line(
                    SharedString::new(entry.text.as_str()),
                    entry.color,
                    window,
                );
                let width = line.width();
                let _ = line.paint(
                    point(x, y),
                    px(HOVER_LABEL_LINE_HEIGHT),
                    TextAlign::Left,
                    None,
                    window,
                    context,
                );
                x += width + px(10.0);
            }
        });
    }

    fn paint_price_axis(
        &mut self,
        ticks: &[PreparedAxisTick],
        lowest: f64,
        highest: f64,
        layout: &ChartLayout,
    ) {
        for tick in ticks {
            let y = y_for_price(tick.value, lowest, highest, layout.price_plot);
            self.window.paint_quad(fill(
                Bounds::new(
                    point(layout.price_plot.origin.x, y),
                    size(layout.price_plot.size.width, px(1.0)),
                ),
                self.context.theme().components.chart.grid.hsla,
            ));
            self.paint_text(
                tick.label.clone(),
                point(
                    layout.price_axis.origin.x + px(7.0),
                    axis_label_y(y, layout.price_axis),
                ),
                self.context.theme().components.chart.axis.hsla,
                TextAlign::Left,
                axis_text_width(layout.price_axis.size.width, AXIS_TICK_TEXT_INSET),
            );
        }
    }

    /// Draws one sub plot: its axis, its guides and its data.
    ///
    /// `series` is `None` for VOL, whose bars are painted with the candles rather
    /// than here; the axis is still drawn for every sub plot.
    ///
    /// The data is wrapped in a content mask because a path can leave the plot by
    /// a pixel: the path takes one candle from the left of the range, and a
    /// histogram bar keeps a one-pixel minimum height.
    fn paint_sub_plot(
        &mut self,
        id: IndicatorId,
        series: Option<&IndicatorSeries>,
        layout: SubPlotLayout,
        range: Range<usize>,
        geometry: CandleGeometry,
        value_range: ValueRange,
    ) {
        let plot = layout.plot;
        let grid = self.context.theme().components.chart.grid.hsla;
        let axis_color = self.context.theme().components.chart.axis.hsla;
        // The axis plan keeps the labels and the solid grid lines apart: a guide
        // is labelled and drawn by its own pass, but it must not also get a grid
        // line, which would fill the gaps of the dashed reference line.
        let guides = series
            .map(|series| series.guides.as_slice())
            .unwrap_or_default();
        let axis = sub_plot_axis(id, value_range, plot.size.height, guides);
        for value in axis.grid {
            let y = y_for_sub_value(value, value_range, plot);
            self.window.paint_quad(fill(
                Bounds::new(point(plot.origin.x, y), size(plot.size.width, px(1.0))),
                grid,
            ));
        }
        for value in axis.labels {
            let y = y_for_sub_value(value, value_range, plot);
            let label = match id {
                IndicatorId::Vol => format_volume(value),
                IndicatorId::Macd => format_signed_price(value, self.price_tick),
                _ => format_percentage_tick(value),
            };
            self.paint_text(
                label,
                point(layout.axis.origin.x + px(7.0), axis_label_y(y, layout.axis)),
                axis_color,
                TextAlign::Left,
                axis_text_width(layout.axis.size.width, AXIS_TICK_TEXT_INSET),
            );
        }

        let Some(series) = series else {
            return;
        };
        for guide in &axis.guides {
            let y = y_for_sub_value(guide.value, value_range, plot);
            if guide.solid {
                // MACD's zero line is a solid axis rather than a reference band.
                self.window.paint_quad(fill(
                    Bounds::new(point(plot.origin.x, y), size(plot.size.width, px(1.0))),
                    grid,
                ));
            } else {
                paint_dashed_horizontal(y, plot, grid, self.window);
            }
        }

        let theme = self.context.theme();
        let bullish = theme.components.chart.bullish.hsla;
        let bearish = theme.components.chart.bearish.hsla;
        let body_width = geometry.body_width();
        let zero_y = y_for_sub_value(0.0, value_range, plot);
        let start = range.start;
        let end = range.end;
        // Histogram bars are painted before the lines so a moving average crossing
        // a bar stays on top of it.
        self.window.paint_layer(plot, |window| {
            window.with_content_mask(Some(ContentMask { bounds: plot }), |window| {
                for line in &series.lines {
                    if line.shape != LineShape::Histogram {
                        continue;
                    }
                    for (slot, absolute) in (start..end).enumerate() {
                        let Some(value) = line.values.get(absolute).copied().flatten() else {
                            continue;
                        };
                        let value_y = y_for_sub_value(value, value_range, plot);
                        let top = value_y.min(zero_y);
                        let height = (value_y - zero_y).abs().max(px(1.0));
                        let x = geometry.center_x(slot) - body_width / 2.0;
                        let color = histogram_color(value, bullish, bearish);
                        window.paint_quad(fill(
                            Bounds::new(point(x, top), size(body_width, height)),
                            color,
                        ));
                    }
                }
            });
        });

        let paths: Vec<(Path<Pixels>, Hsla)> = series
            .lines
            .iter()
            .filter(|line| line.shape == LineShape::Line)
            .filter_map(|line| {
                build_indicator_path(&line.values, range.clone(), geometry, |value| {
                    y_for_sub_value(value, value_range, plot)
                })
                .map(|path| (path, line.color))
            })
            .collect();
        if paths.is_empty() {
            return;
        }
        self.window.paint_layer(plot, |window| {
            window.with_content_mask(Some(ContentMask { bounds: plot }), |window| {
                for (path, color) in paths {
                    window.paint_path(path, color);
                }
            });
        });
    }

    fn paint_time_axis(&mut self, ticks: &[PreparedTimeTick], layout: &ChartLayout) {
        let data_bounds = layout.data_bounds();
        for tick in ticks {
            self.window.paint_quad(fill(
                Bounds::new(
                    point(tick.x, data_bounds.origin.y),
                    size(px(1.0), data_bounds.size.height),
                ),
                self.context.theme().components.chart.grid.hsla,
            ));
            self.paint_text(
                tick.label.clone(),
                point(tick.x - px(34.0), layout.time_axis.origin.y + px(6.0)),
                self.context.theme().components.chart.axis.hsla,
                TextAlign::Center,
                px(68.0),
            );
        }
    }

    fn paint_latest_price_indicator(
        &mut self,
        latest: &PreparedLatestPrice,
        countdown_label: SharedString,
        layout: &ChartLayout,
    ) {
        let y = latest.y;
        let color = if latest.is_up {
            self.context.theme().components.chart.bullish.hsla
        } else {
            self.context.theme().components.chart.bearish.hsla
        };
        paint_dashed_horizontal(y, layout.price_plot, color, self.window);
        self.paint_latest_price_axis_tag(
            y,
            latest.label.clone(),
            countdown_label,
            layout.price_axis,
            color,
        );
    }

    fn paint_latest_price_axis_tag(
        &mut self,
        y: Pixels,
        price_label: SharedString,
        countdown_label: SharedString,
        axis: Bounds<Pixels>,
        background: Hsla,
    ) {
        let tag_height = px(LATEST_PRICE_TAG_HEIGHT).min(axis.size.height);
        if tag_height <= px(0.0) {
            return;
        }
        let tag_y = (y - px(LATEST_PRICE_TAG_PRICE_HEIGHT / 2.0))
            .max(axis.origin.y)
            .min(axis.bottom() - tag_height);
        self.window.paint_quad(fill(
            Bounds::new(
                point(axis.origin.x, tag_y),
                size(axis.size.width, tag_height),
            ),
            background,
        ));

        let text_width = axis_text_width(axis.size.width, AXIS_TAG_TEXT_INSET);
        if tag_height < px(34.0) {
            self.paint_text(
                price_label,
                point(
                    axis.origin.x + px(5.0),
                    tag_y + (tag_height - px(HOVER_LABEL_LINE_HEIGHT)).max(px(0.0)) / 2.0,
                ),
                self.context.theme().roles.fg.on_brand.hsla,
                TextAlign::Left,
                text_width,
            );
            return;
        }

        self.paint_text(
            price_label,
            point(axis.origin.x + px(5.0), tag_y + px(3.0)),
            self.context.theme().roles.fg.on_brand.hsla,
            TextAlign::Left,
            text_width,
        );
        self.paint_text(
            countdown_label,
            point(
                axis.origin.x + px(5.0),
                tag_y + px(LATEST_PRICE_TAG_PRICE_HEIGHT),
            ),
            self.context.theme().roles.fg.on_brand.hsla,
            TextAlign::Left,
            text_width,
        );
    }

    fn paint_crosshair(
        &mut self,
        snapshot: &CandlestickSeriesSnapshot,
        visible: &PreparedVisibleChart,
        interval: TimeFrame,
        zone: UtcOffset,
        cursor: Point<Pixels>,
        layout: &ChartLayout,
        hover_label_cache: &Rc<RefCell<HoverLabelCache>>,
        hover_label_top: Pixels,
    ) {
        let Some(slot) = visible.geometry.slot_at(cursor.x) else {
            return;
        };
        let candles = snapshot.candles();
        let price_tick = snapshot.price_tick();
        let candle_index = visible.range.start + slot;
        let x = visible.geometry.center_x(slot);
        paint_dashed_vertical(
            x,
            layout.data_bounds(),
            self.context.theme().components.chart.crosshair.hsla,
            self.window,
        );

        if layout.price_plot.contains(&cursor) {
            let y = cursor
                .y
                .clamp(layout.price_plot.origin.y, layout.price_plot.bottom());
            paint_dashed_horizontal(
                y,
                layout.price_plot,
                self.context.theme().components.chart.crosshair.hsla,
                self.window,
            );
            let price = price_for_y(y, visible.lowest, visible.highest, layout.price_plot);
            self.paint_axis_tag(y, format_price(price, price_tick), layout.price_axis);
        } else if let Some((index, sub_plot)) = layout
            .sub_plots
            .iter()
            .enumerate()
            .find(|(_, sub_plot)| sub_plot.plot.contains(&cursor))
        {
            let y = cursor
                .y
                .clamp(sub_plot.plot.origin.y, sub_plot.plot.bottom());
            paint_dashed_horizontal(
                y,
                sub_plot.plot,
                self.context.theme().components.chart.crosshair.hsla,
                self.window,
            );
            if let Some(prepared_sub_plot) = visible.sub_plots.get(index) {
                let label = match prepared_sub_plot.id {
                    IndicatorId::Vol => format_volume(volume_for_y(
                        y,
                        prepared_sub_plot.value_range.highest,
                        sub_plot.plot,
                    )),
                    IndicatorId::Macd => format_signed_price(
                        sub_value_from_y(y, prepared_sub_plot.value_range, sub_plot.plot),
                        price_tick,
                    ),
                    _ => format_percentage_value(sub_value_from_y(
                        y,
                        prepared_sub_plot.value_range,
                        sub_plot.plot,
                    )),
                };
                self.paint_axis_tag(y, label, sub_plot.axis);
            }
        }

        let candle = &candles[candle_index];
        let previous = candle_index
            .checked_sub(1)
            .and_then(|index| candles.get(index));
        // The label the size is read in is the hovered candle's own unit, so a
        // candle that published none carries the missing state instead of a unit.
        let volume_unit = traded_unit_label(candle, self.context);
        let presentation = hover_label_cache.borrow_mut().presentation(
            candle,
            previous,
            price_tick,
            interval,
            zone,
            &volume_unit,
            self.context.theme(),
            self.window,
        );
        let tag_width = px(132.0).min(layout.time_axis.size.width);
        let tag_x = (x - tag_width / 2.0)
            .max(layout.time_axis.origin.x)
            .min(layout.time_axis.right() - tag_width);
        self.window.paint_quad(fill(
            Bounds::new(
                point(tag_x, layout.time_axis.origin.y),
                size(tag_width, layout.time_axis.size.height),
            ),
            self.context.theme().roles.brand.base.hsla,
        ));
        self.paint_text(
            presentation.time_label,
            point(tag_x, layout.time_axis.origin.y + px(6.0)),
            self.context.theme().roles.fg.on_brand.hsla,
            TextAlign::Center,
            tag_width,
        );
        paint_hover_label(
            presentation.line.as_ref(),
            layout.price_plot,
            hover_label_top,
            self.window,
            self.context,
        );
    }

    fn paint_axis_tag(&mut self, y: Pixels, label: impl AsRef<str>, axis: Bounds<Pixels>) {
        let tag_height = px(20.0).min(axis.size.height);
        let tag_y = (y - tag_height / 2.0)
            .max(axis.origin.y)
            .min(axis.bottom() - tag_height);
        self.window.paint_quad(fill(
            Bounds::new(
                point(axis.origin.x, tag_y),
                size(axis.size.width, tag_height),
            ),
            self.context.theme().roles.brand.base.hsla,
        ));
        self.paint_text(
            label,
            point(axis.origin.x + px(5.0), tag_y + px(3.0)),
            self.context.theme().roles.fg.on_brand.hsla,
            TextAlign::Left,
            axis_text_width(axis.size.width, AXIS_TAG_TEXT_INSET),
        );
    }

    fn paint_text(
        &mut self,
        text: impl AsRef<str>,
        origin: Point<Pixels>,
        color: Hsla,
        align: TextAlign,
        width: Pixels,
    ) {
        let line =
            self.text_cache
                .borrow_mut()
                .line(SharedString::new(text.as_ref()), color, self.window);
        let _ = line.paint(
            origin,
            px(HOVER_LABEL_LINE_HEIGHT),
            align,
            Some(width),
            self.window,
            self.context,
        );
    }
}

/// The candle the latest-price indicator points at, if it is on screen.
fn latest_candle_for_indicator(
    snapshot: &CandlestickSeriesSnapshot,
    visible_end: usize,
) -> Option<&ListingCandlestick> {
    if !latest_price_indicator_is_visible(
        snapshot.candles().len(),
        visible_end,
        snapshot.at_live_edge(),
    ) {
        return None;
    }
    snapshot.latest()
}

/// Whether the latest price belongs on the visible chart.
///
/// It is drawn only while the chart is at the live edge and the newest candle is
/// close enough to be meaningful: an indicator for a candle far off screen would
/// have to be clamped onto the axis and would then point at a price that is not
/// there.
pub(crate) fn latest_price_indicator_is_visible(
    candle_count: usize,
    visible_end: usize,
    at_live_edge: bool,
) -> bool {
    candle_count > 0
        && at_live_edge
        && candle_count.saturating_sub(visible_end) <= LATEST_PRICE_MAX_CANDLE_GAP
}

/// The raw high/low span of `candles`, with the latest price folded in.
fn price_extremes(candles: &[ListingCandlestick], latest_price: Option<Price>) -> (f64, f64) {
    let mut highest = candles
        .iter()
        .map(|candle| CandlePrices::of(candle).high().inner() as f64)
        .fold(0.0_f64, f64::max);
    let mut lowest = candles
        .iter()
        .map(|candle| CandlePrices::of(candle).low().inner() as f64)
        .fold(f64::INFINITY, f64::min);
    if let Some(latest_price) = latest_price {
        let latest_price = latest_price.inner() as f64;
        highest = highest.max(latest_price);
        lowest = lowest.min(latest_price);
    }
    (lowest, highest)
}

/// Pads raw extremes so that nothing touches the plot edges.
///
/// Every source of a bound has to be folded in **before** this runs, or the
/// padding is computed from a narrower range than the one being drawn.
fn padded_price_bounds(lowest: f64, highest: f64) -> (f64, f64) {
    let range = (highest - lowest).max(1.0);
    (lowest - range * 0.04, highest + range * 0.04)
}

/// The price range to draw, padded so that candles do not touch the plot edges.
///
/// The painted chart always goes through [`price_bounds_with_indicators`], even
/// when no indicator is enabled; this plain form is what the bounds tests pin,
/// so it stays test-only rather than growing a production caller with the same
/// body.
#[cfg(test)]
fn price_bounds(candles: &[ListingCandlestick], latest_price: Option<Price>) -> (f64, f64) {
    let (lowest, highest) = price_extremes(candles, latest_price);
    padded_price_bounds(lowest, highest)
}

/// The main-chart price range, including every enabled main-chart indicator
/// value in `range`.
///
/// `candles` is the **whole snapshot** and `range` indexes it; the series'
/// values are indexed the same way. Mixing a relative slice with an absolute
/// range has no consistency constraint at all (a 200-candle slice with
/// `1300..1500` would pass unnoticed), which is why the signature only accepts
/// absolute indices.
///
/// The indicators have to be included because a moving average's window can
/// cross the left edge of the visible range: the MA500 of the leftmost visible
/// candle averages closes from candles the chart is not drawing, which may be
/// above every visible high. Merging them after padding would leave the line
/// outside the range it was padded from — and `with_content_mask` would cut it
/// off at the plot edge.
fn price_bounds_with_indicators(
    candles: &[ListingCandlestick],
    range: Range<usize>,
    latest_price: Option<Price>,
    main_series: &[IndicatorSeries],
) -> (f64, f64) {
    debug_assert!(range.start <= range.end);
    debug_assert!(
        range.end <= candles.len(),
        "the range indexes the whole snapshot, got {range:?} for {} candles",
        candles.len()
    );

    let (mut lowest, mut highest) = price_extremes(&candles[range.clone()], latest_price);
    for series in main_series {
        if definition_of(series.id).pane != IndicatorPane::Main {
            continue;
        }
        for line in &series.lines {
            debug_assert_eq!(
                line.values.len(),
                candles.len(),
                "an indicator line is indexed like the whole snapshot"
            );
            for value in line.values[range.clone()].iter().flatten() {
                lowest = lowest.min(*value);
                highest = highest.max(*value);
            }
        }
    }
    padded_price_bounds(lowest, highest)
}

/// The absolute candle index the legend reads.
///
/// It is the candle under the pointer, or the newest visible one when the
/// pointer is away. The visible slot alone is not an index into an
/// `IndicatorLine`: after a pan, slot `n` belongs to another candle, so the
/// addition of `range.start` is required (design §5.6).
fn legend_candle_index(
    cursor: Option<Point<Pixels>>,
    layout: &ChartLayout,
    visible: &PreparedVisibleChart,
) -> Option<usize> {
    let slot = cursor
        .filter(|cursor| layout.contains_data_point(cursor))
        .and_then(|cursor| visible.geometry.slot_at(cursor.x))
        .unwrap_or(visible.range.len().checked_sub(1)?);
    Some(visible.range.start + slot)
}

/// One entry of a chart legend.
struct LegendEntry {
    text: SmolStr,
    color: Hsla,
}

/// The main-chart legend entries for the candle at `absolute`.
///
/// A line contributes an entry only once the candle is past its own
/// `readable_from` threshold: the value is defined earlier for EMA and MACD, but
/// it is not yet trustworthy.
fn main_indicator_legend(
    indicators: &[IndicatorSeries],
    absolute: usize,
    price_tick: Tick,
) -> Vec<LegendEntry> {
    let mut entries = Vec::new();
    for series in indicators {
        if definition_of(series.id).pane != IndicatorPane::Main {
            continue;
        }
        for line in &series.lines {
            if absolute < line.readable_from {
                continue;
            }
            let Some(value) = line.values.get(absolute).copied().flatten() else {
                continue;
            };
            entries.push(LegendEntry {
                text: format_smolstr!("{} {}", line.label, format_price(value, price_tick)),
                color: line.color,
            });
        }
    }
    entries
}

/// The legend entries of one sub plot for the candle at `absolute`.
///
/// The index is absolute, like the values it reads: a visible slot would read
/// another candle's values after a pan. A line contributes only past its own
/// `readable_from`; the formula name is drawn even when no value is defined yet.
fn sub_plot_legend(
    id: IndicatorId,
    indicators: &[IndicatorSeries],
    absolute: usize,
    snapshot: &CandlestickSeriesSnapshot,
    price_tick: Tick,
    axis_color: Hsla,
) -> Vec<LegendEntry> {
    if id == IndicatorId::Vol {
        let text = snapshot.candles().get(absolute).map_or_else(
            || SmolStr::new_static("VOL"),
            |candle| match &candle.traded_quantity {
                Some(quantity) => {
                    format_smolstr!("VOL {}", format_volume(quantity.value.inner() as f64))
                }
                None => SmolStr::new_static("VOL —"),
            },
        );
        return vec![LegendEntry {
            text,
            color: axis_color,
        }];
    }

    let Some(series) = indicators.iter().find(|series| series.id == id) else {
        return Vec::new();
    };
    let mut entries = Vec::with_capacity(series.lines.len() + 1);
    // RSI's line labels already begin with `RSI`, so repeating the series name
    // would read `RSI RSI6`. The other sub plots show the formula with its
    // parameters, which a line label does not carry.
    if id != IndicatorId::Rsi {
        entries.push(LegendEntry {
            text: series.legend.clone(),
            color: axis_color,
        });
    }
    for line in &series.lines {
        if line.shape != LineShape::Line || absolute < line.readable_from {
            continue;
        }
        let Some(value) = line.values.get(absolute).copied().flatten() else {
            continue;
        };
        let text = match id {
            IndicatorId::Macd => {
                format_smolstr!("{} {}", line.label, format_signed_price(value, price_tick))
            }
            _ => format_smolstr!("{} {}", line.label, format_percentage_value(value)),
        };
        entries.push(LegendEntry {
            text,
            color: line.color,
        });
    }
    entries
}

/// Builds one indicator line's path over the visible range.
///
/// `values` is indexed like the **whole snapshot** and `range` is the visible
/// absolute range, so the two are one coordinate system. The path takes one
/// candle from the left of the range as well, so the segment entering the plot
/// starts at the right height; the right end takes nothing extra, because the
/// future is not drawn. A `None` value breaks the line there: no segment is
/// fabricated across a gap.
fn build_indicator_path(
    values: &[Option<f64>],
    range: Range<usize>,
    geometry: CandleGeometry,
    y_for: impl Fn(f64) -> Pixels,
) -> Option<Path<Pixels>> {
    debug_assert!(range.start <= range.end);
    debug_assert!(
        range.end <= values.len(),
        "the range indexes the whole snapshot, got {range:?} for {} values",
        values.len()
    );

    let mut builder = PathBuilder::stroke(px(1.0));
    let mut started = false;
    let mut segments = 0usize;
    let first = range.start.saturating_sub(1);
    for (index, value) in values.iter().enumerate().take(range.end).skip(first) {
        match *value {
            Some(value) => {
                let x = if index < range.start {
                    // The extra candle is outside `visible_len`, so `center_x`
                    // has no slot for it.
                    geometry.center_x_left_of_first(range.start - index)
                } else {
                    geometry.center_x(index - range.start)
                };
                let to = point(x, y_for(value));
                if started {
                    builder.line_to(to);
                    segments += 1;
                } else {
                    builder.move_to(to);
                    started = true;
                }
            }
            None => started = false,
        }
    }
    if segments == 0 {
        return None;
    }
    builder.build().ok()
}

/// The tallest traded size the volume plot has to fit.
///
/// Only traded sizes are plotted: an amount is denominated in another unit, so a
/// candle that published one and no traded size has no bar rather than a bar of a
/// different quantity.
fn volume_upper_bound(candles: &[ListingCandlestick]) -> f64 {
    let highest = candles
        .iter()
        .filter_map(|candle| candle.traded_quantity.as_ref())
        .map(|quantity| quantity.value.inner() as f64)
        .fold(0.0_f64, f64::max);
    let one_quantity = Quantity::from(1_u64).inner() as f64;
    if highest == 0.0 {
        one_quantity
    } else {
        (highest * VOLUME_HEADROOM).min(u128::MAX as f64)
    }
}

/// The prices the y-axis shows, at a spacing that keeps the labels apart.
fn price_axis_values(lowest: f64, highest: f64, plot_height: Pixels, price_tick: Tick) -> Vec<f64> {
    let range = highest - lowest;
    if !range.is_finite() || range <= 0.0 {
        return vec![lowest];
    }

    let plot_height = f64::from(f32::from(plot_height).max(0.0));
    if plot_height < f64::from(PRICE_AXIS_MIN_LABEL_SPACING) {
        return vec![(lowest + highest) / 2.0];
    }

    let target_intervals = (plot_height / f64::from(PRICE_AXIS_TARGET_LABEL_SPACING))
        .round()
        .clamp(1.0, MAX_PRICE_AXIS_TARGET_INTERVALS);
    // Nearest-step selection targets visual density; the ceiling step enforces
    // the hard minimum gap.
    let preferred_step = nearest_nice_step(range / target_intervals);
    let minimum_step = nice_step(range * f64::from(PRICE_AXIS_MIN_LABEL_SPACING) / plot_height);
    let step = align_step_to_price_tick(preferred_step.max(minimum_step), price_tick);
    let first = (lowest / step).ceil() * step;
    let mut values = Vec::with_capacity(MAX_PRICE_AXIS_TICKS);
    let mut value = first;
    while value <= highest && values.len() < MAX_PRICE_AXIS_TICKS {
        values.push(value);
        let next = value + step;
        if next <= value {
            break;
        }
        value = next;
    }
    if values.is_empty() {
        values.push((lowest + highest) / 2.0);
    }
    values
}

/// Rounds a step up to a whole number of price ticks.
///
/// The axis must only ever label prices the market can actually trade at, or two
/// labels would round to the same string at coarse ticks.
fn align_step_to_price_tick(step: f64, price_tick: Tick) -> f64 {
    let step = step.ceil().clamp(1.0, u128::MAX as f64) as u128;
    let tick = price_tick.inner();
    step.div_ceil(tick)
        .checked_mul(tick)
        .unwrap_or((u128::MAX / tick) * tick) as f64
}

fn volume_axis_values(upper: f64) -> Vec<f64> {
    let step = nice_step(upper / 3.0);
    let mut values = Vec::with_capacity(8);
    let mut value = 0.0;
    while value <= upper && values.len() < 8 {
        values.push(value);
        value += step;
    }
    values
}

/// The y range one sub plot draws for the visible window.
///
/// `volume_upper` is passed in because the caller has already read the visible
/// candles for the axis candidates; recomputing it here would walk them twice.
fn sub_plot_value_range(
    id: IndicatorId,
    volume_upper: f64,
    range: Range<usize>,
    indicators: &[IndicatorSeries],
) -> ValueRange {
    match id {
        IndicatorId::Vol => ValueRange {
            lowest: 0.0,
            highest: volume_upper,
        },
        _ => {
            let series = indicators
                .iter()
                .find(|series| series.id == id)
                .expect("every enabled sub-plot indicator has a computed series");
            sub_value_range(series, range)
        }
    }
}

/// The y range of one computed sub plot over the visible window (design §6.4).
///
/// It is derived every frame from `range` rather than stored on the series: a
/// range computed over every loaded candle would flatten the visible curve when
/// a distant candle moved the extremes.
fn sub_value_range(series: &IndicatorSeries, range: Range<usize>) -> ValueRange {
    debug_assert!(range.start <= range.end);
    let Some(first_line) = series.lines.first() else {
        // KDJ with every line switched off still owns a sub plot; there is
        // nothing to measure, so it falls back to the percentage domain.
        return ValueRange {
            lowest: 0.0,
            highest: 100.0,
        };
    };
    debug_assert!(
        range.end <= first_line.values.len(),
        "the range indexes the whole snapshot, got {range:?} for {} values",
        first_line.values.len()
    );

    match series.id {
        IndicatorId::Macd => {
            let mut magnitude = 0.0_f64;
            for line in &series.lines {
                for value in line.values[range.clone()].iter().flatten() {
                    magnitude = magnitude.max(value.abs());
                }
            }
            // An all-zero MACD collapses onto the middle line; a zero span would
            // make the y mapping divide by zero.
            let magnitude = magnitude.max(1e-9);
            ValueRange {
                lowest: -magnitude,
                highest: magnitude,
            }
        }
        IndicatorId::Rsi => ValueRange {
            lowest: 0.0,
            highest: 100.0,
        },
        IndicatorId::Kdj => {
            let mut lowest = 0.0_f64;
            let mut highest = 100.0_f64;
            let mut any = false;
            for line in &series.lines {
                for value in line.values[range.clone()].iter().flatten() {
                    any = true;
                    lowest = lowest.min(*value);
                    highest = highest.max(*value);
                }
            }
            if !any {
                return ValueRange {
                    lowest: 0.0,
                    highest: 100.0,
                };
            }
            // A negative J is kept: clamping the lower bound at zero would draw
            // it outside the box (design D11).
            let padding = (highest - lowest) * 0.05;
            ValueRange {
                lowest: lowest - padding,
                highest: highest + padding,
            }
        }
        // A sub plot only ever shows a computed sub-plot indicator; a percentage
        // is the only sensible default for a value that is not a price or a size.
        IndicatorId::Ma | IndicatorId::Ema | IndicatorId::Vol => ValueRange {
            lowest: 0.0,
            highest: 100.0,
        },
    }
}

/// The values a sub plot labels, chosen for its height.
fn sub_axis_values(id: IndicatorId, value_range: ValueRange, plot_height: Pixels) -> Vec<f64> {
    let compact = f32::from(plot_height) < SUB_PLOT_COMPACT_HEIGHT;
    match id {
        IndicatorId::Vol => volume_axis_values(value_range.highest),
        IndicatorId::Macd => vec![value_range.lowest, 0.0, value_range.highest],
        IndicatorId::Rsi => {
            if compact {
                vec![30.0, 50.0, 70.0]
            } else {
                vec![0.0, 30.0, 50.0, 70.0, 100.0]
            }
        }
        IndicatorId::Kdj => {
            if compact {
                vec![0.0, 50.0, 100.0]
            } else {
                kdj_axis_values(value_range)
            }
        }
        // The main-chart indicators are never painted in a sub plot; the two
        // bounds keep the function total for any caller that asks anyway.
        IndicatorId::Ma | IndicatorId::Ema => vec![value_range.lowest, value_range.highest],
    }
}

/// One reference line of a sub plot.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SubPlotGuide {
    value: f64,
    /// Whether the line is solid (MACD's zero axis) or dashed (a reference band).
    solid: bool,
}

/// Everything a sub plot's y-axis draws, with the labels and the solid grid
/// lines kept apart.
///
/// A guide value is labelled by `labels` and painted by `guides`, but it must not
/// also appear in `grid`: a full-width solid line at the same y would fill the
/// gaps of the dashed reference line and make it read as an ordinary axis line.
/// The plan is derived here rather than inside the paint loop so the split is
/// testable without capturing pixels.
#[derive(Debug, PartialEq)]
struct SubPlotAxis {
    labels: Vec<f64>,
    grid: Vec<f64>,
    guides: Vec<SubPlotGuide>,
}

fn sub_plot_axis(
    id: IndicatorId,
    value_range: ValueRange,
    plot_height: Pixels,
    guides: &[f64],
) -> SubPlotAxis {
    let labels = sub_axis_values(id, value_range, plot_height);
    let grid = labels
        .iter()
        .copied()
        .filter(|value| {
            !guides
                .iter()
                .any(|guide| (*guide - *value).abs() < f64::EPSILON)
        })
        .collect();
    let guides = guides
        .iter()
        .map(|value| SubPlotGuide {
            value: *value,
            solid: id == IndicatorId::Macd,
        })
        .collect();
    SubPlotAxis {
        labels,
        grid,
        guides,
    }
}

/// KDJ's labels: 0/20/80/100 plus whichever dynamic extreme left the domain.
///
/// The design asks for 3–5 labels. [`sub_value_range`] pads the 0..100 domain by
/// 5% on each side, so a padded span is `1.1x` the data span and `span / 22` is
/// exactly that padding. A bound that sits one padding step outside 0 / 100 is
/// therefore the padding itself rather than a J that left the domain; labelling
/// it would stack a tick on top of the 0 / 100 label.
///
/// If J goes below zero *and* above 100 there are six candidates; the fixed
/// endpoint closer to its neighbour is dropped, because those two labels would
/// sit on top of each other while the 20 / 80 guides must survive.
fn kdj_axis_values(value_range: ValueRange) -> Vec<f64> {
    let span = value_range.highest - value_range.lowest;
    let padding = span / 22.0;
    let mut values = vec![0.0, 20.0, 80.0, 100.0];
    if -value_range.lowest > padding + f64::EPSILON {
        values.push(value_range.lowest);
    }
    if value_range.highest - 100.0 > padding + f64::EPSILON {
        values.push(value_range.highest);
    }
    values.sort_by(f64::total_cmp);
    values.dedup_by(|a, b| (*a - *b).abs() < f64::EPSILON);
    if values.len() > 5 {
        let below_zero = -value_range.lowest;
        let above_hundred = value_range.highest - 100.0;
        if below_zero <= above_hundred {
            values.retain(|value| *value != 0.0);
        } else {
            values.retain(|value| *value != 100.0);
        }
    }
    values
}

/// Formats a 0–100 axis label. Negative zero reads as `0`, not `-0`.
fn format_percentage_tick(value: f64) -> SmolStr {
    if value == 0.0 {
        SmolStr::new_static("0")
    } else {
        format_smolstr!("{value:.0}")
    }
}

/// Formats a 0–100 readout such as a legend value or a crosshair tag.
///
/// A small negative J rounds to `-0.0`, which reads as a negative value that is
/// not there; the rounded form is normalised instead.
fn format_percentage_value(value: f64) -> SmolStr {
    if value == 0.0 {
        SmolStr::new_static("0.0")
    } else {
        let text = format_smolstr!("{value:.1}");
        if text.as_str() == "-0.0" {
            SmolStr::new_static("0.0")
        } else {
            text
        }
    }
}

/// Maps a sub-plot value to a y coordinate inside `plot`.
fn y_for_sub_value(value: f64, value_range: ValueRange, plot: Bounds<Pixels>) -> Pixels {
    let span = (value_range.highest - value_range.lowest).max(f64::MIN_POSITIVE);
    let ratio = ((value_range.highest - value) / span).clamp(0.0, 1.0);
    plot.origin.y + plot.size.height * ratio as f32
}

/// The sub-plot value a y coordinate stands for; the inverse of
/// [`y_for_sub_value`].
fn sub_value_from_y(y: Pixels, value_range: ValueRange, plot: Bounds<Pixels>) -> f64 {
    let ratio = if plot.size.height > px(0.0) {
        (f32::from(y - plot.origin.y) / f32::from(plot.size.height)) as f64
    } else {
        0.0
    };
    value_range.highest - (value_range.highest - value_range.lowest) * ratio.clamp(0.0, 1.0)
}

/// Paints the visible candles' wicks, volume bars and bodies.
///
/// `hovered` is the visible slot under the pointer. Its volume bar takes the
/// candle's own color at full strength, so the bar the user is reading stands out
/// while the rest of the volume plot keeps the weaker tone.
fn paint_candle_series(
    candles: &[PreparedCandle],
    hovered: Option<usize>,
    layout: &ChartLayout,
    theme: &Theme,
    window: &mut Window,
) {
    let bullish = theme.roles.market.up.hsla;
    let bearish = theme.roles.market.down.hsla;
    let bullish_weak = theme.roles.market.up_weak.hsla;
    let bearish_weak = theme.roles.market.down_weak.hsla;
    // Wicks and volume bars do not overlap within their respective slot/plot, so
    // they can share one scene order. Bodies overlap their own wicks and
    // therefore require a second layer.
    window.paint_layer(layout.data_bounds(), |window| {
        for (index, candle) in candles.iter().enumerate() {
            let color = if candle.is_up { bullish } else { bearish };
            window.paint_quad(fill(candle.wick, color));
            if let Some(bounds) = candle.volume {
                let (weak, strong) = if candle.is_up {
                    (bullish_weak, bullish)
                } else {
                    (bearish_weak, bearish)
                };
                let volume = if hovered == Some(index) { strong } else { weak };
                window.paint_quad(fill(bounds, volume));
            }
        }
    });
    window.paint_layer(layout.price_plot, |window| {
        for candle in candles {
            let color = if candle.is_up { bullish } else { bearish };
            window.paint_quad(fill(candle.body, color));
        }
    });
}

fn candle_wick_bounds(
    prices: CandlePrices,
    center_x: Pixels,
    lowest: f64,
    highest: f64,
    plot: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let high_y = y_for_price(prices.high().inner() as f64, lowest, highest, plot);
    let low_y = y_for_price(prices.low().inner() as f64, lowest, highest, plot);
    Bounds::new(
        point(center_x - px(0.5), high_y),
        size(px(1.0), (low_y - high_y).max(px(1.0))),
    )
}

fn candle_body_bounds(
    prices: CandlePrices,
    center_x: Pixels,
    body_width: Pixels,
    lowest: f64,
    highest: f64,
    plot: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let open_y = y_for_price(prices.open().inner() as f64, lowest, highest, plot);
    let close_y = y_for_price(prices.close().inner() as f64, lowest, highest, plot);
    Bounds::new(
        point(center_x - body_width / 2.0, open_y.min(close_y)),
        size(body_width, (close_y - open_y).abs().max(px(1.0))),
    )
}

/// The bar of one candle's traded size, absent when it published none.
fn candle_volume_bounds(
    candle: &ListingCandlestick,
    center_x: Pixels,
    body_width: Pixels,
    upper: f64,
    plot: Option<Bounds<Pixels>>,
) -> Option<Bounds<Pixels>> {
    let plot = plot?;
    let quantity = candle.traded_quantity.as_ref()?;
    if quantity.value.is_zero() {
        return None;
    }
    let top = y_for_volume(quantity.value.inner() as f64, upper, plot);
    Some(Bounds::new(
        point(center_x - body_width / 2.0, top),
        size(body_width, (plot.bottom() - top).max(px(1.0))),
    ))
}

/// The text one candle's traded size is read in.
///
/// A candle that published no traded size has no unit to name, so the label is
/// empty and the size itself reads as the missing state.
fn traded_unit_label(candle: &ListingCandlestick, cx: &App) -> SharedString {
    match &candle.traded_quantity {
        Some(quantity) => unit_label(&quantity.unit, cx),
        None => SharedString::new_static(""),
    }
}

/// The text one quantity unit is labelled with.
///
/// A traded size is labelled with the asset it counts, a contract count with the
/// shared name for contracts, and an outcome count with the shared name for
/// outcome tokens. The asset name is the catalog's own term for it, which is why
/// the label is read here rather than from the venue's field names. An amount is
/// named after the asset it is denominated in; a traded size is never one, so
/// that arm is what keeps the match total rather than a case the chart draws.
fn unit_label(unit: &QuantityUnit, cx: &App) -> SharedString {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_label(asset, cx)
        }
        QuantityUnit::Contracts => t!(cx, "widget.candlestick.contracts"),
        QuantityUnit::OutcomeTokens { .. } => t!(cx, "widget.candlestick.outcome_tokens"),
    }
}

/// The display code of one asset, as the published generation decided it.
///
/// The catalog is the only place a canonical [`AssetId`] becomes a name, and the code
/// it publishes is the whole name of the asset. An asset the published generation
/// does not name is the localized unknown: an identity digest is not a name this
/// chart may print, and a contract address is not one either.
fn asset_label(asset: &AssetId, cx: &App) -> SharedString {
    cx.global::<MarketParticipantsConnector>()
        .catalog()
        .cached_display()
        .resolver()
        .asset_code(asset)
        .map_or_else(
            |_| t!(cx, "error.display.unknown_asset"),
            |code| SharedString::new(code.as_str()),
        )
}

fn hover_label_bounds(price_plot: Bounds<Pixels>, top_inset: Pixels) -> Bounds<Pixels> {
    let inset = px(HOVER_LABEL_INSET);
    Bounds::new(
        point(price_plot.origin.x + inset, price_plot.origin.y + top_inset),
        size(
            (price_plot.size.width - inset * 2.0).max(px(0.0)),
            (price_plot.size.height - top_inset - inset)
                .max(px(0.0))
                .min(px(HOVER_LABEL_LINE_HEIGHT)),
        ),
    )
}

fn paint_hover_label(
    line: &ShapedLine,
    price_plot: Bounds<Pixels>,
    top_inset: Pixels,
    window: &mut Window,
    context: &mut App,
) {
    let bounds = hover_label_bounds(price_plot, top_inset);
    if bounds.size.width <= px(0.0) || bounds.size.height <= px(0.0) {
        return;
    }

    // Clipped to the price plot: the label is as wide as the candle data, which
    // is wider than a narrow pane.
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        let _ = line.paint(
            bounds.origin,
            px(HOVER_LABEL_LINE_HEIGHT),
            TextAlign::Left,
            Some(bounds.size.width),
            window,
            context,
        );
    });
}

fn paint_dashed_horizontal(y: Pixels, plot: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    window.paint_layer(plot, |window| {
        for_each_horizontal_dash(y, plot, |bounds| {
            window.paint_quad(fill(bounds, color));
        });
    });
}

fn for_each_horizontal_dash(y: Pixels, plot: Bounds<Pixels>, mut f: impl FnMut(Bounds<Pixels>)) {
    let mut x = plot.origin.x;
    while x < plot.right() {
        let width = px(5.0).min(plot.right() - x);
        f(Bounds::new(point(x, y), size(width, px(1.0))));
        x += px(9.0);
    }
}

fn paint_dashed_vertical(x: Pixels, plot: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    window.paint_layer(plot, |window| {
        for_each_vertical_dash(x, plot, |bounds| {
            window.paint_quad(fill(bounds, color));
        });
    });
}

fn for_each_vertical_dash(x: Pixels, plot: Bounds<Pixels>, mut f: impl FnMut(Bounds<Pixels>)) {
    let mut y = plot.origin.y;
    while y < plot.bottom() {
        let height = px(5.0).min(plot.bottom() - y);
        f(Bounds::new(point(x, y), size(px(1.0), height)));
        y += px(9.0);
    }
}

fn y_for_price(price: f64, lowest: f64, highest: f64, plot: Bounds<Pixels>) -> Pixels {
    let ratio = ((highest - price) / (highest - lowest).max(1.0)).clamp(0.0, 1.0);
    plot.origin.y + plot.size.height * ratio as f32
}

fn price_for_y(y: Pixels, lowest: f64, highest: f64, plot: Bounds<Pixels>) -> f64 {
    let ratio = (f32::from(y - plot.origin.y) / f32::from(plot.size.height)) as f64;
    highest - (highest - lowest) * ratio.clamp(0.0, 1.0)
}

fn y_for_volume(volume: f64, upper: f64, plot: Bounds<Pixels>) -> Pixels {
    let ratio = (volume / upper.max(1.0)).clamp(0.0, 1.0);
    plot.bottom() - plot.size.height * ratio as f32
}

fn volume_for_y(y: Pixels, upper: f64, plot: Bounds<Pixels>) -> f64 {
    let ratio = (f32::from(plot.bottom() - y) / f32::from(plot.size.height)) as f64;
    upper * ratio.clamp(0.0, 1.0)
}

fn axis_label_y(y: Pixels, axis: Bounds<Pixels>) -> Pixels {
    let label_height = px(HOVER_LABEL_LINE_HEIGHT).min(axis.size.height);
    (y - label_height / 2.0)
        .max(axis.origin.y)
        .min(axis.bottom() - label_height)
}

/// Rounds a step up to the next readable increment.
fn nice_step(raw_step: f64) -> f64 {
    if !raw_step.is_finite() || raw_step <= 0.0 {
        return 1.0;
    }
    let magnitude = 10_f64.powf(raw_step.log10().floor());
    let normalized = raw_step / magnitude;
    let nice = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 2.5 {
        2.5
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

/// Rounds a step to the nearest readable increment.
fn nearest_nice_step(raw_step: f64) -> f64 {
    if !raw_step.is_finite() || raw_step <= 0.0 {
        return 1.0;
    }
    let magnitude = 10_f64.powf(raw_step.log10().floor());
    let normalized = raw_step / magnitude;
    let nice = if normalized < 1.5 {
        1.0
    } else if normalized < 2.25 {
        2.0
    } else if normalized < 3.75 {
        2.5
    } else if normalized < 7.5 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use gpui_kit::{Context, Render, TestAppContext, div};
    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        decimal::{Price, Quantity},
        listing::{rules::ListingPrecision, venue::PriceBasis},
        market::{
            book::ListingCandlestick,
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
        outcome::{OutcomeGroupId, OutcomeMemberId, OutcomeSideId},
    };

    use super::*;
    use crate::indicator::IndicatorLine;

    fn market_price(value: Price) -> MarketPrice {
        MarketPrice {
            value,
            quote_asset: test_support::currency("USD"),
            basis: PriceBasis::PerAssetUnit,
        }
    }

    fn candle(timestamp: i64) -> ListingCandlestick {
        ListingCandlestick {
            listing: test_support::listing_key(&test_support::spot_route(), "BTCUSDT"),
            open: market_price(Price::from(1_u64)),
            high: market_price(Price::from(2_u64)),
            low: market_price(Price::from(1_u64)),
            close: market_price(Price::from(2_u64)),
            traded_quantity: Some(MarketQuantity {
                value: Quantity::from(1_u64),
                unit: QuantityUnit::Contracts,
            }),
            traded_notional: None,
            timestamp,
        }
    }

    fn candle_with_volume(timestamp: i64, volume: Quantity) -> ListingCandlestick {
        let mut candle = candle(timestamp);
        candle.traded_quantity = Some(MarketQuantity {
            value: volume,
            unit: QuantityUnit::Contracts,
        });
        candle
    }

    fn candle_with_prices(
        timestamp: i64,
        open: u64,
        high: u64,
        low: u64,
        close: u64,
    ) -> ListingCandlestick {
        ListingCandlestick {
            open: market_price(Price::from(open)),
            high: market_price(Price::from(high)),
            low: market_price(Price::from(low)),
            close: market_price(Price::from(close)),
            ..candle(timestamp)
        }
    }

    fn test_line(
        label: &'static str,
        values: Vec<Option<f64>>,
        readable_from: usize,
        shape: LineShape,
    ) -> IndicatorLine {
        IndicatorLine {
            label: SmolStr::new_static(label),
            color: gpui_kit::white(),
            values,
            readable_from,
            shape,
        }
    }

    fn test_series(
        id: IndicatorId,
        legend: &'static str,
        lines: Vec<IndicatorLine>,
        guides: Vec<f64>,
    ) -> IndicatorSeries {
        IndicatorSeries {
            id,
            legend: SmolStr::new_static(legend),
            lines,
            guides,
        }
    }

    fn snapshot_of(
        candles: impl IntoIterator<Item = ListingCandlestick>,
    ) -> CandlestickSeriesSnapshot {
        let candles: Vec<ListingCandlestick> = candles.into_iter().collect();
        let mut series = crate::model::CandlestickSeries::new(
            candles.len().max(1),
            ListingPrecision {
                price_tick: Tick::from_str("0.01").expect("price tick should parse"),
                quantity_tick: Tick::from_str("0.001").expect("quantity step should parse"),
            },
        );
        series.merge_initial(candles);
        series.snapshot()
    }

    fn test_tick() -> Tick {
        Tick::from_str("0.01").expect("tick should parse")
    }

    #[test]
    fn candle_series_layers_should_keep_geometry_non_overlapping() {
        let rising = candle(1);
        let falling = candle_with_prices(2, 2, 2, 1, 1);
        let zero_volume = {
            let mut candle = candle(3);
            candle.traded_quantity = Some(MarketQuantity {
                value: Quantity::ZERO,
                unit: QuantityUnit::Contracts,
            });
            candle
        };
        let missing_volume = {
            let mut candle = candle(4);
            candle.traded_quantity = None;
            candle
        };
        let candles = [rising, falling, zero_volume, missing_volume];
        let (lowest, highest) = price_bounds(&candles, None);
        let volume_upper = volume_upper_bound(&candles);
        let layout = ChartLayout::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0))),
            LayoutInputs {
                longest_label_width: px(0.0),
                sub_plot_count: 1,
            },
        );
        let volume_plot = layout.sub_plots[0].plot;
        let geometry = CandleGeometry::new(layout.price_plot, candles.len(), 5.0);
        let body_width = geometry.body_width();
        let mut wick_and_volume_bounds = Vec::with_capacity(candles.len() * 2);
        let mut body_bounds = Vec::with_capacity(candles.len());

        for (index, candle) in candles.iter().enumerate() {
            let center_x = geometry.center_x(index);
            wick_and_volume_bounds.push(candle_wick_bounds(
                CandlePrices::of(candle),
                center_x,
                lowest,
                highest,
                layout.price_plot,
            ));
            wick_and_volume_bounds.extend(candle_volume_bounds(
                candle,
                center_x,
                body_width,
                volume_upper,
                Some(volume_plot),
            ));
            body_bounds.push(candle_body_bounds(
                CandlePrices::of(candle),
                center_x,
                body_width,
                lowest,
                highest,
                layout.price_plot,
            ));
        }

        assert!(
            wick_and_volume_bounds
                .iter()
                .enumerate()
                .all(|(index, bounds)| {
                    wick_and_volume_bounds[index + 1..]
                        .iter()
                        .all(|other| !bounds.intersects(other))
                })
        );
        assert!(body_bounds.iter().enumerate().all(|(index, bounds)| {
            body_bounds[index + 1..]
                .iter()
                .all(|other| !bounds.intersects(other))
        }));
        assert!(
            candle_volume_bounds(
                &candles[2],
                geometry.center_x(2),
                body_width,
                volume_upper,
                Some(volume_plot),
            )
            .is_none()
        );
        assert!(
            candle_volume_bounds(
                &candles[3],
                geometry.center_x(3),
                body_width,
                volume_upper,
                Some(volume_plot),
            )
            .is_none(),
            "a candle that published no traded size has no bar"
        );
    }

    #[test]
    fn dashed_segments_should_preserve_count_and_bounds() {
        let plot = Bounds::new(point(px(10.0), px(20.0)), size(px(20.0), px(20.0)));
        let mut horizontal = Vec::with_capacity(3);
        let mut vertical = Vec::with_capacity(3);
        for_each_horizontal_dash(px(25.0), plot, |bounds| horizontal.push(bounds));
        for_each_vertical_dash(px(15.0), plot, |bounds| vertical.push(bounds));

        assert_eq!(
            horizontal,
            [
                Bounds::new(point(px(10.0), px(25.0)), size(px(5.0), px(1.0))),
                Bounds::new(point(px(19.0), px(25.0)), size(px(5.0), px(1.0))),
                Bounds::new(point(px(28.0), px(25.0)), size(px(2.0), px(1.0))),
            ]
        );
        assert_eq!(
            vertical,
            [
                Bounds::new(point(px(15.0), px(20.0)), size(px(1.0), px(5.0))),
                Bounds::new(point(px(15.0), px(29.0)), size(px(1.0), px(5.0))),
                Bounds::new(point(px(15.0), px(38.0)), size(px(1.0), px(2.0))),
            ]
        );
    }

    #[test]
    fn latest_price_indicator_should_be_visible_at_forty_candle_gap() {
        assert!(latest_price_indicator_is_visible(100, 60, true));
    }

    #[test]
    fn latest_price_indicator_should_be_hidden_beyond_forty_candle_gap() {
        assert!(!latest_price_indicator_is_visible(100, 59, true));
    }

    #[test]
    fn latest_price_indicator_should_be_hidden_away_from_live_edge() {
        assert!(!latest_price_indicator_is_visible(100, 100, false));
    }

    #[test]
    fn latest_price_indicator_should_be_hidden_without_candles() {
        assert!(!latest_price_indicator_is_visible(0, 0, true));
    }

    #[test]
    fn nice_step_should_use_readable_price_increment() {
        assert_eq!(nice_step(2.2), 2.5);
    }

    #[test]
    fn nearest_nice_step_should_choose_the_closest_readable_increment() {
        assert_eq!(nearest_nice_step(2.2), 2.0);
    }

    #[test]
    fn align_step_to_price_tick_should_use_whole_tick_multiples() {
        let tick = Tick::from_inner(30).expect("price tick should be positive");

        assert_eq!(align_step_to_price_tick(100.0, tick), 120.0);
    }

    #[test]
    fn price_axis_values_should_add_labels_for_taller_plots() {
        let tick = Tick::from_inner(1).expect("price tick should be positive");
        let short = price_axis_values(0.0, 1_000.0, px(200.0), tick);
        let tall = price_axis_values(0.0, 1_000.0, px(480.0), tick);

        assert!(tall.len() > short.len());
    }

    #[test]
    fn price_axis_values_should_use_compact_nice_steps_for_typical_height() {
        let tick = Tick::from_inner(1).expect("price tick should be positive");
        let values = price_axis_values(0.0, 1_000.0, px(360.0), tick);

        assert_eq!(values[1] - values[0], 100.0);
    }

    #[test]
    fn price_axis_values_should_keep_minimum_label_spacing() {
        let tick = Tick::from_inner(1).expect("price tick should be positive");
        let height = px(360.0);
        let values = price_axis_values(0.0, 1_000.0, height, tick);

        assert!(values.windows(2).all(|pair| {
            let spacing = (pair[1] - pair[0]) / 1_000.0 * f64::from(f32::from(height));
            spacing >= f64::from(PRICE_AXIS_MIN_LABEL_SPACING)
        }));
    }

    #[test]
    fn price_axis_values_should_use_one_label_when_plot_is_too_short() {
        let tick = Tick::from_inner(1).expect("price tick should be positive");

        assert_eq!(price_axis_values(0.0, 1_000.0, px(20.0), tick).len(), 1);
    }

    #[test]
    fn price_axis_values_should_not_repeat_labels_for_coarse_price_ticks() {
        let tick = Tick::from_inner(250).expect("price tick should be positive");
        let labels = price_axis_values(0.0, 1_000.0, px(480.0), tick)
            .into_iter()
            .map(|value| format_price(value, tick))
            .collect::<Vec<_>>();

        assert!(labels.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn hover_label_bounds_should_not_exceed_narrow_price_plot() {
        let plot = Bounds::new(point(px(10.0), px(20.0)), size(px(6.0), px(10.0)));
        let label = hover_label_bounds(plot, px(HOVER_LABEL_INSET));

        assert_eq!((label.size.width, label.size.height), (px(0.0), px(2.0)));
    }

    /// Without a legend the OHLC label keeps the plot's original top inset.
    #[test]
    fn hover_label_top_should_keep_the_original_inset_without_a_legend() {
        assert_eq!(hover_label_top(false), px(HOVER_LABEL_INSET));
    }

    /// With a legend the OHLC label starts one legend row (plus the gap) lower.
    #[test]
    fn hover_label_top_should_move_below_the_legend_row_when_it_is_drawn() {
        assert_eq!(
            hover_label_top(true),
            px(HOVER_LABEL_INSET + HOVER_LABEL_LINE_HEIGHT + HOVER_LABEL_LEGEND_GAP)
        );
        assert!(hover_label_top(true) > hover_label_top(false));
    }

    /// The legend keeps the plot's top-left corner, and the hover label starts
    /// below the legend row instead of being painted over it.
    #[test]
    fn hover_label_should_start_below_the_legend_row() {
        let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
        let without_legend = hover_label_bounds(plot, hover_label_top(false));
        let with_legend = hover_label_bounds(plot, hover_label_top(true));

        assert_eq!(
            without_legend.origin.y,
            plot.origin.y + px(HOVER_LABEL_INSET),
            "without a legend the label keeps the original top inset"
        );
        assert!(
            with_legend.origin.y >= plot.origin.y + px(HOVER_LABEL_INSET + HOVER_LABEL_LINE_HEIGHT),
            "the OHLC label must not start inside the legend's line"
        );
        assert!(
            with_legend.origin.y > without_legend.origin.y,
            "the label moves down only while the legend is drawn"
        );
        assert_eq!(with_legend.size.height, px(HOVER_LABEL_LINE_HEIGHT));
    }

    #[test]
    fn axis_text_width_should_not_be_negative_for_narrow_axes() {
        assert_eq!(axis_text_width(px(4.0), AXIS_TICK_TEXT_INSET), px(0.0));
    }

    #[test]
    fn volume_axis_candidates_should_cover_unit_transition_labels() {
        let candles = [candle_with_volume(1, Quantity::from(1_000_000_u64))];
        let tick = Tick::from_str("1").expect("price tick should parse");
        let labels = axis_label_candidates(
            &candles,
            0..candles.len(),
            None,
            tick,
            px(300.0),
            &[],
            &[IndicatorId::Vol],
        );

        assert!(
            labels.iter().any(|label| label == "999.999K")
                && labels.iter().any(|label| label == "1000.000K"),
            "unit-transition candidates were {labels:?}"
        );
    }

    #[test]
    fn volume_upper_bound_should_fall_back_to_one_for_zero_volume() {
        let candles = [candle_with_volume(1, Quantity::ZERO)];

        assert_eq!(
            volume_upper_bound(&candles),
            Quantity::from(1_u64).inner() as f64
        );
    }

    /// The plot is scaled by traded sizes only: a monetary amount is another
    /// quantity, and a chart that plotted it would be drawing two units on one axis.
    #[test]
    fn volume_upper_bound_should_ignore_candles_without_a_traded_size() {
        let missing = {
            let mut candle = candle(1);
            candle.traded_quantity = None;
            candle.traded_notional = Some(MarketQuantity {
                value: Quantity::from(1_000_000_u64),
                unit: QuantityUnit::Notional {
                    asset: test_support::currency("USD"),
                },
            });
            candle
        };
        let candles = [missing, candle_with_volume(2, Quantity::from(4_u64))];

        let upper = volume_upper_bound(&candles);

        assert!(upper >= 4.0, "the traded size scales the plot: {upper}");
        assert!(
            upper < Quantity::from(1_000_000_u64).inner() as f64,
            "an amount must not scale the traded-size plot: {upper}"
        );
    }

    /// A traded size is labelled with the asset it counts, and the label is the
    /// display code the published generation decided on rather than a field name the
    /// venue chose. An asset that generation does not name is the localized
    /// unknown — never its canonical identity, which is a digest and not a name.
    #[gpui_kit::test]
    fn unit_labels_should_name_the_asset_or_the_shared_unit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            widget_tokio_runtime::global::install(cx).expect("test Tokio runtime should build");
            // A pair that is denominated in both assets the test names: a display
            // code is only published for an asset some published listing uses.
            let directory = test_support::Fixture::new(vec![test_support::entry(
                &test_support::spot_route(),
                test_support::spot(
                    test_support::token(test_support::VENUE, "BTC"),
                    test_support::currency("USD"),
                ),
                "BTCUSD",
            )]);
            widget_tokio_runtime::global::handle(cx)
                .handle()
                .block_on(directory.ensure())
                .expect("the fixture spot route should publish its catalog");
            market_participants_connector::test_support::install_root(
                cx,
                std::sync::Arc::clone(directory.catalog_arc()),
            );
        });

        cx.update(|cx| {
            let unknown = test_support::token("unknown-issuer", "ZZZ");
            let outcome = QuantityUnit::OutcomeTokens {
                group: OutcomeGroupId::from_identity_value(&format!("v1/{}", "a".repeat(64)))
                    .expect("the fixture group is canonical"),
                member: OutcomeMemberId::from_identity_value(&format!("v1/{}", "b".repeat(64)))
                    .expect("the fixture member is canonical"),
                side: OutcomeSideId::from_identity_value(&format!("v1/{}", "c".repeat(64)))
                    .expect("the fixture side is canonical"),
            };

            assert_eq!(
                [
                    unit_label(
                        &QuantityUnit::AssetUnits {
                            asset: test_support::token(test_support::VENUE, "BTC"),
                        },
                        cx,
                    ),
                    unit_label(
                        &QuantityUnit::AssetUnits {
                            asset: test_support::currency("USD"),
                        },
                        cx,
                    ),
                    unit_label(&QuantityUnit::Contracts, cx),
                    unit_label(&outcome, cx),
                    unit_label(&QuantityUnit::AssetUnits { asset: unknown }, cx),
                ],
                [
                    SharedString::new_static("BTC"),
                    SharedString::new_static("USD"),
                    t!(cx, "widget.candlestick.contracts"),
                    t!(cx, "widget.candlestick.outcome_tokens"),
                    t!(cx, "error.display.unknown_asset"),
                ]
            );
        });
    }

    #[test]
    fn zero_volume_should_map_to_plot_bottom() {
        let plot = Bounds::new(point(px(10.0), px(20.0)), size(px(400.0), px(100.0)));

        assert_eq!(y_for_volume(0.0, 100.0, plot), plot.bottom());
    }

    #[test]
    fn maximum_volume_should_map_to_plot_top() {
        let plot = Bounds::new(point(px(10.0), px(20.0)), size(px(400.0), px(100.0)));

        assert_eq!(y_for_volume(100.0, 100.0, plot), plot.origin.y);
    }

    #[test]
    fn volume_hover_midpoint_should_map_to_half_the_upper_bound() {
        let plot = Bounds::new(point(px(10.0), px(20.0)), size(px(400.0), px(100.0)));
        let midpoint = plot.origin.y + plot.size.height / 2.0;

        assert_eq!(volume_for_y(midpoint, 100.0, plot), 50.0);
    }

    #[test]
    fn price_bounds_should_pad_the_visible_range() {
        let candles = [
            candle_with_prices(1, 100, 110, 90, 100),
            candle_with_prices(2, 100, 104, 96, 100),
        ];

        let (lowest, highest) = price_bounds(&candles, None);

        assert!(
            lowest < Price::from(90_u64).inner() as f64,
            "the lowest candle must not touch the plot edge"
        );
        assert!(
            highest > Price::from(110_u64).inner() as f64,
            "the highest candle must not touch the plot edge"
        );
    }

    #[test]
    fn price_bounds_should_include_a_latest_price_above_every_visible_candle() {
        let candles = [candle(1)];
        let latest = Price::from(100_u64);

        let (_, highest) = price_bounds(&candles, Some(latest));

        assert!(highest > latest.inner() as f64);
    }

    /// The prepared chart is the expensive half of a frame, so the key that
    /// decides whether it may be reused has to notice every input that changes
    /// what it contains. Each case below is one way a frame can differ.
    #[test]
    fn a_prepared_chart_key_should_reject_every_changed_drawing_input() {
        let snapshot = one_candle_snapshot();
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
        let viewport = ChartViewport {
            candle_width: 9.0,
            right_timestamp: None,
            cursor: None,
        };
        let text_color = gpui_kit::white();
        let other_text_color = gpui_kit::black();
        let indicator_config = CandlestickIndicatorConfig::default();
        let key = PreparedChartKey {
            snapshot: snapshot.clone(),
            candle_width: viewport.candle_width,
            right_timestamp: viewport.right_timestamp,
            interval: TimeFrame::Minute1,
            zone: UtcOffset::UTC,
            bounds,
            text_style: TextStyle::default(),
            text_color,
            indicator_config: indicator_config.clone(),
            panel_open: false,
        };
        let unchanged = |snapshot: &CandlestickSeriesSnapshot,
                         viewport: ChartViewport,
                         interval: TimeFrame,
                         zone: UtcOffset,
                         bounds: Bounds<Pixels>,
                         text_color: Hsla| {
            key.matches(
                snapshot,
                viewport,
                interval,
                zone,
                bounds,
                &TextStyle::default(),
                text_color,
                &indicator_config,
                false,
            )
        };

        assert!(unchanged(
            &snapshot,
            viewport,
            TimeFrame::Minute1,
            UtcOffset::UTC,
            bounds,
            text_color
        ));
        assert!(
            !unchanged(
                &snapshot,
                ChartViewport {
                    candle_width: 10.0,
                    ..viewport
                },
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                text_color,
            ),
            "zooming changes the candle boxes"
        );
        assert!(
            !unchanged(
                &snapshot,
                ChartViewport {
                    right_timestamp: Some(1),
                    ..viewport
                },
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                text_color,
            ),
            "panning changes which candles are visible"
        );
        assert!(
            !unchanged(
                &snapshot,
                viewport,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                Bounds::new(point(px(0.0), px(0.0)), size(px(640.0), px(480.0))),
                text_color,
            ),
            "resizing changes every box"
        );
        assert!(
            !unchanged(
                &snapshot,
                viewport,
                TimeFrame::Minute5,
                UtcOffset::UTC,
                bounds,
                text_color,
            ),
            "the interval changes both time labels and the label stride"
        );
        assert!(
            !unchanged(
                &snapshot,
                viewport,
                TimeFrame::Minute1,
                UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset"),
                bounds,
                text_color,
            ),
            "another display zone changes every time label"
        );
        assert!(
            !unchanged(
                &snapshot,
                viewport,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                other_text_color
            ),
            "a theme change changes the measured label colours"
        );
        assert!(
            !unchanged(
                &one_candle_snapshot(),
                viewport,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                text_color,
            ),
            "a new snapshot changes the candles"
        );
        let mut other_config = indicator_config.clone();
        other_config.entries[0].enabled = false;
        assert!(
            !key.matches(
                &snapshot,
                viewport,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                &TextStyle::default(),
                text_color,
                &other_config,
                false,
            ),
            "switching a main-chart indicator off changes the lines and the bounds"
        );
        assert!(
            !key.matches(
                &snapshot,
                viewport,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                &TextStyle::default(),
                text_color,
                &indicator_config,
                true,
            ),
            "opening the panel changes the chart width"
        );
    }

    /// A two-point run over the left edge proves both halves of the path
    /// contract: the extra candle is drawn, and a `None` never gets a segment
    /// fabricated across it.
    #[test]
    fn build_indicator_path_should_break_at_a_none_and_take_one_candle_from_the_left() {
        let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
        let geometry = CandleGeometry::new(plot, 2, 10.0);
        let y_for = |value: f64| px(value as f32);

        assert!(
            build_indicator_path(&[Some(1.0), None, Some(3.0)], 0..3, geometry, y_for).is_none(),
            "a gap leaves two isolated points, which are not a line"
        );
        assert!(
            build_indicator_path(&[Some(1.0), Some(3.0)], 0..1, geometry, y_for).is_none(),
            "the right end takes no extra candle"
        );

        let path = build_indicator_path(&[Some(1.0), Some(3.0)], 1..2, geometry, y_for)
            .expect("a two-point run is a line");
        assert!(
            path.bounds.origin.x < geometry.center_x(0),
            "the path starts at the candle left of the first visible slot, got {:?}",
            path.bounds
        );
        assert!(
            path.bounds.right() >= geometry.center_x(0),
            "the path reaches the first visible slot, got {:?}",
            path.bounds
        );
    }

    /// Passing a relative slice with an absolute range has no consistency
    /// constraint, so the contract is checked at the entry.
    #[test]
    #[should_panic(expected = "the range indexes the whole snapshot")]
    fn build_indicator_path_should_reject_a_relative_slice() {
        let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
        let geometry = CandleGeometry::new(plot, 2, 10.0);

        let _ = build_indicator_path(&[Some(1.0), Some(2.0)], 1..3, geometry, |value| {
            px(value as f32)
        });
    }

    /// The extra candle an indicator path draws is one slot left of the first
    /// visible one, which `center_x` cannot address because that slot is outside
    /// `visible_len`. A chart with no visible candle must still answer it.
    #[test]
    fn build_indicator_path_should_take_one_candle_width_left_of_the_first_slot() {
        let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
        let geometry = CandleGeometry::new(plot, 2, 10.0);

        assert_eq!(
            geometry.center_x_left_of_first(1),
            geometry.center_x(0) - px(10.0)
        );
        assert_eq!(geometry.center_x_left_of_first(1), px(75.0));

        let empty = CandleGeometry::new(plot, 0, 10.0);
        assert_eq!(
            empty.center_x_left_of_first(1),
            empty.center_x(0) - px(10.0)
        );
    }

    /// A range whose start is past its end is not a visible window; the debug
    /// assertion names it instead of silently producing an empty path.
    #[test]
    #[should_panic(expected = "range.start <= range.end")]
    fn build_indicator_path_should_reject_a_reversed_range() {
        let plot = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
        let geometry = CandleGeometry::new(plot, 2, 10.0);
        // Built from variables so the lint that flags a literal `2..1` does not
        // fire: the reversed range is the input this test is about.
        let start = 2;
        let end = 1;

        let _ = build_indicator_path(&[Some(1.0), Some(2.0)], start..end, geometry, |value| {
            px(value as f32)
        });
    }

    /// The padding is computed once, from the range that already contains the
    /// indicator values; padding the candle range first and merging afterwards
    /// would leave the line outside the box it was padded for.
    #[test]
    fn price_bounds_with_indicators_should_pad_after_merging_the_values() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];
        let scale = Price::from(1_u64).inner() as f64;
        let series = [IndicatorSeries {
            id: crate::indicator::IndicatorId::Ma,
            legend: SmolStr::new_static("MA"),
            lines: vec![IndicatorLine {
                label: SmolStr::new_static("MA5"),
                color: gpui_kit::white(),
                values: vec![Some(Price::from(200_u64).inner() as f64)],
                readable_from: 0,
                shape: LineShape::Line,
            }],
            guides: Vec::new(),
        }];

        let (lowest, highest) = price_bounds_with_indicators(&candles, 0..1, None, &series);

        // Raw span 90..200, padded by 4% of 110 on each side.
        assert!((lowest / scale - 85.6).abs() < 1e-9, "lowest was {lowest}");
        assert!(
            (highest / scale - 204.4).abs() < 1e-9,
            "highest was {highest}"
        );
        // The plain bounds keep their existing behaviour.
        let (plain_lowest, plain_highest) = price_bounds(&candles, None);
        assert!(
            plain_lowest < Price::from(90_u64).inner() as f64,
            "plain lowest {plain_lowest}"
        );
        assert!(
            plain_highest > Price::from(110_u64).inner() as f64,
            "plain highest {plain_highest}"
        );
    }

    /// The range indexes the whole snapshot, so a range that reaches past it
    /// means the caller passed a relative slice; that is a contract violation,
    /// not an empty chart.
    #[test]
    #[should_panic(expected = "the range indexes the whole snapshot")]
    fn price_bounds_with_indicators_should_reject_a_relative_slice() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];

        let _ = price_bounds_with_indicators(&candles, 0..2, None, &[]);
    }

    /// Only main-chart lines move the price range; a sub-plot value (here a MACD
    /// histogram far outside the candles) must leave it alone. No indicators at
    /// all is the same case as every main indicator switched off.
    #[test]
    fn price_bounds_with_indicators_should_ignore_sub_plot_series() {
        let candles = [
            candle_with_prices(1, 100, 110, 90, 100),
            candle_with_prices(2, 100, 104, 96, 100),
        ];
        let plain = price_bounds(&candles, None);
        let sub_series = [IndicatorSeries {
            id: crate::indicator::IndicatorId::Macd,
            legend: SmolStr::new_static("MACD(12,26,9)"),
            lines: vec![IndicatorLine {
                label: SmolStr::new_static("MACD"),
                color: gpui_kit::white(),
                values: vec![Some(1.0e9); candles.len()],
                readable_from: 0,
                shape: LineShape::Histogram,
            }],
            guides: vec![0.0],
        }];

        assert_eq!(
            price_bounds_with_indicators(&candles, 0..candles.len(), None, &sub_series),
            plain,
            "a sub-plot value is not part of the main-chart price range"
        );
        assert_eq!(
            price_bounds_with_indicators(&candles, 0..candles.len(), None, &[]),
            plain,
            "no main-chart series is the same range as the plain candle bounds"
        );
    }

    /// The axis is sized from these candidates, so a main-chart line that moved
    /// the price range must add its own label to the set; otherwise the axis
    /// could be too narrow for the tick the plot draws.
    #[test]
    fn axis_label_candidates_should_include_main_indicator_values() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];
        let tick = Tick::from_str("0.01").expect("tick should parse");
        let series = [IndicatorSeries {
            id: crate::indicator::IndicatorId::Ma,
            legend: SmolStr::new_static("MA"),
            lines: vec![IndicatorLine {
                label: SmolStr::new_static("MA5"),
                color: gpui_kit::white(),
                values: vec![Some(Price::from(200_u64).inner() as f64)],
                readable_from: 0,
                shape: LineShape::Line,
            }],
            guides: Vec::new(),
        }];

        let (_, highest) = price_bounds_with_indicators(&candles, 0..1, None, &series);
        let with_indicator =
            axis_label_candidates(&candles, 0..1, None, tick, px(300.0), &series, &[]);
        let baseline = axis_label_candidates(&candles, 0..1, None, tick, px(300.0), &[], &[]);

        assert!(
            with_indicator.contains(&format_price(highest, tick)),
            "the candidate set must contain the indicator-driven upper bound {highest}"
        );
        assert!(
            !baseline.contains(&format_price(highest, tick)),
            "the candle-only candidates must not contain the indicator-driven bound"
        );
    }

    struct CacheHarness;

    impl Render for CacheHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn snapshot_with_candles(count: usize) -> CandlestickSeriesSnapshot {
        let mut series = crate::model::CandlestickSeries::new(
            count,
            ListingPrecision {
                price_tick: Tick::from_str("0.01").expect("price tick should parse"),
                quantity_tick: Tick::from_str("0.001").expect("quantity step should parse"),
            },
        );
        series.merge_initial(
            (0..count).map(|index| candle_with_prices(index as i64 + 1, 100, 110, 90, 100)),
        );
        series.snapshot()
    }

    /// Panning moves the prepared chart's key but not the inputs of the value
    /// computation, so the values must be reused. This is the whole reason the
    /// two caches are separate (design §7.3).
    #[gpui_kit::test]
    fn panning_one_candle_should_not_recompute_the_indicator_values(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });
        let (_, cx) = cx.add_window_view(|_, _| CacheHarness);
        let snapshot = snapshot_with_candles(100);
        let config = CandlestickIndicatorConfig::default();
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
        let viewport = ChartViewport::default();
        let panned = ChartViewport {
            right_timestamp: Some(snapshot.candles()[79].timestamp),
            ..viewport
        };

        cx.update(|window, _| {
            let caches = ChartPaintCaches::default();
            let first = prepare_chart_cached(
                &caches,
                &snapshot,
                viewport,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                gpui_kit::white(),
                &config,
                false,
                window,
            );
            assert_eq!(caches.indicators.borrow().builds, 1);
            assert_eq!(caches.prepared.borrow().builds, 1);
            assert!(!first.indicators.is_empty(), "MA is enabled by default");

            let second = prepare_chart_cached(
                &caches,
                &snapshot,
                panned,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                gpui_kit::white(),
                &config,
                false,
                window,
            );
            assert_eq!(
                caches.indicators.borrow().builds,
                1,
                "panning must not recompute the values"
            );
            assert_eq!(
                caches.prepared.borrow().builds,
                2,
                "panning must rebuild the layout and the candle boxes"
            );
            assert!(Arc::ptr_eq(&first.indicators, &second.indicators));
        });
    }

    /// The sub-plot count and order come from the configuration, not from the
    /// computed series: VOL has no computed values but is still a sub plot. The
    /// layout, the prepared data and the painting loop must all agree on that
    /// answer, and enabling a second indicator has to reach the layout.
    #[gpui_kit::test]
    fn the_layout_should_draw_one_box_per_enabled_sub_plot(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });
        let (_, cx) = cx.add_window_view(|_, _| CacheHarness);
        let snapshot = snapshot_with_candles(100);
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
        let viewport = ChartViewport::default();
        let mut with_volume = CandlestickIndicatorConfig::default();
        with_volume
            .entries
            .iter_mut()
            .find(|entry| entry.id == IndicatorId::Macd)
            .expect("the catalogue declares MACD")
            .enabled = true;

        cx.update(|window, _| {
            let caches = ChartPaintCaches::default();
            let prepared = prepare_chart_cached(
                &caches,
                &snapshot,
                viewport,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                gpui_kit::white(),
                &with_volume,
                false,
                window,
            );

            assert_eq!(
                prepared.layout.sub_plots.len(),
                2,
                "VOL and MACD are both enabled, so the layout needs two boxes"
            );
            let visible = prepared.visible.as_ref().expect("candles are visible");
            assert_eq!(
                visible.sub_plots.len(),
                prepared.layout.sub_plots.len(),
                "every box has exactly one prepared sub plot"
            );
            assert_eq!(visible.sub_plots[0].id, IndicatorId::Vol);
            assert_eq!(visible.sub_plots[1].id, IndicatorId::Macd);
            assert!(
                visible.candles.iter().all(|candle| candle.volume.is_some()),
                "the VOL box owns the prepared volume bars"
            );
        });

        // Turning VOL off leaves MACD as the first and only sub plot; no volume
        // bar may be prepared for its box.
        let mut without_volume = CandlestickIndicatorConfig::default();
        for entry in &mut without_volume.entries {
            entry.enabled = matches!(entry.id, IndicatorId::Ma | IndicatorId::Macd);
        }

        cx.update(|window, _| {
            let caches = ChartPaintCaches::default();
            let prepared = prepare_chart_cached(
                &caches,
                &snapshot,
                viewport,
                TimeFrame::Minute1,
                UtcOffset::UTC,
                bounds,
                gpui_kit::white(),
                &without_volume,
                false,
                window,
            );

            assert_eq!(prepared.layout.sub_plots.len(), 1, "only MACD is enabled");
            let visible = prepared.visible.as_ref().expect("candles are visible");
            assert_eq!(visible.sub_plots.len(), 1);
            assert_eq!(
                visible.sub_plots[0].id,
                IndicatorId::Macd,
                "MACD takes the first box; no empty box is left for VOL"
            );
            assert!(
                visible.candles.iter().all(|candle| candle.volume.is_none()),
                "MACD's box must not receive volume bars"
            );
        });
    }

    /// The legend's value is read at the absolute index the crosshair is on,
    /// and only past that line's own `readable_from`.
    #[test]
    fn the_main_legend_should_read_the_candle_under_the_cursor() {
        let layout = ChartLayout::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0))),
            LayoutInputs {
                longest_label_width: px(0.0),
                sub_plot_count: 1,
            },
        );
        let geometry = CandleGeometry::new(layout.price_plot, 10, 9.0);
        let visible = PreparedVisibleChart {
            range: 10..20,
            lowest: 0.0,
            highest: 100.0,
            geometry,
            candles: Arc::from(Vec::new()),
            price_ticks: Arc::from(Vec::new()),
            time_ticks: Arc::from(Vec::new()),
            latest_price: None,
            sub_plots: Arc::from(Vec::new()),
        };
        let cursor = point(geometry.center_x(3), layout.price_plot.origin.y + px(5.0));

        assert_eq!(
            legend_candle_index(Some(cursor), &layout, &visible),
            Some(13),
            "slot 3 of a range starting at 10 is candle 13"
        );
        assert_eq!(
            legend_candle_index(None, &layout, &visible),
            Some(19),
            "without a cursor the legend reads the newest visible candle"
        );

        let price = Price::from_str("123.45").expect("price should parse");
        let tick = Tick::from_str("0.01").expect("tick should parse");
        let mut values = vec![None; 20];
        values[13] = Some(price.inner() as f64);
        let mut series = [IndicatorSeries {
            id: crate::indicator::IndicatorId::Ma,
            legend: SmolStr::new_static("MA"),
            lines: vec![IndicatorLine {
                label: SmolStr::new_static("MA5"),
                color: gpui_kit::white(),
                values,
                readable_from: 14,
                shape: LineShape::Line,
            }],
            guides: Vec::new(),
        }];
        let absolute = legend_candle_index(Some(cursor), &layout, &visible)
            .expect("the cursor is over the plot");

        assert!(
            main_indicator_legend(&series, absolute, tick).is_empty(),
            "a candle below readable_from has no trustworthy value"
        );
        series[0].lines[0].readable_from = absolute;
        let entries = main_indicator_legend(&series, absolute, tick);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "MA5 123.45");
    }

    /// A chart with every sub-plot indicator switched off has no sub-plot boxes
    /// at all: the price plot takes the content and the time axis sits directly
    /// under it.
    #[test]
    fn a_chart_without_enabled_sub_plots_should_have_no_sub_plot_boxes() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
        let layout = ChartLayout::new(
            bounds,
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 0,
            },
        );

        assert!(layout.sub_plots.is_empty());
        assert!(layout.separators.is_empty());
        assert_eq!(layout.time_axis.origin.y, layout.price_plot.bottom());
        assert_eq!(layout.data_bounds(), layout.price_plot);
    }

    #[test]
    fn no_enabled_sub_plot_should_prepare_no_sub_plot_data_or_volume_bars() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0))),
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 0,
            },
        );
        let indicators: Arc<[IndicatorSeries]> = Arc::from(Vec::new());

        let prepared = prepare_visible_chart(
            &snapshot,
            ChartViewport::default(),
            TimeFrame::Minute1,
            UtcOffset::UTC,
            layout,
            indicators,
            &[],
        );
        let visible = prepared.visible.expect("ten candles are visible");

        assert!(visible.sub_plots.is_empty());
        assert!(
            visible.candles.iter().all(|candle| candle.volume.is_none()),
            "no VOL sub plot means no volume bars"
        );
    }

    /// A pane too short for sub plots keeps the price plot's floor and draws no
    /// sub-plot box at all. Nothing may be prepared for boxes that do not exist:
    /// the painter indexes the prepared list by the layout's boxes.
    #[test]
    fn a_pane_too_short_for_sub_plots_should_prepare_none() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(60.0))),
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 4,
            },
        );
        assert!(
            layout.sub_plots.is_empty(),
            "60px leaves no room for a sub plot"
        );

        let prepared = prepare_visible_chart(
            &snapshot,
            ChartViewport::default(),
            TimeFrame::Minute1,
            UtcOffset::UTC,
            layout,
            Arc::from(Vec::new()),
            &[
                IndicatorId::Vol,
                IndicatorId::Macd,
                IndicatorId::Rsi,
                IndicatorId::Kdj,
            ],
        );
        let visible = prepared.visible.expect("ten candles are visible");

        assert!(visible.sub_plots.is_empty());
        assert!(
            visible.candles.iter().all(|candle| candle.volume.is_none()),
            "a sub plot that is not drawn owns no volume bars"
        );
    }

    /// Only a VOL first sub plot owns the volume bars. When MACD is the only
    /// enabled sub plot the bars must not be drawn into its box.
    #[test]
    fn a_first_sub_plot_that_is_not_volume_should_not_prepare_volume_bars() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0))),
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 1,
            },
        );
        let series = test_series(
            IndicatorId::Macd,
            "MACD(12,26,9)",
            vec![test_line(
                "MACD",
                vec![Some(1.0); snapshot.candles().len()],
                0,
                LineShape::Histogram,
            )],
            vec![0.0],
        );
        let indicators: Arc<[IndicatorSeries]> = Arc::from(vec![series]);

        let prepared = prepare_visible_chart(
            &snapshot,
            ChartViewport::default(),
            TimeFrame::Minute1,
            UtcOffset::UTC,
            layout,
            indicators,
            &[IndicatorId::Macd],
        );
        let visible = prepared.visible.expect("ten candles are visible");

        assert_eq!(visible.sub_plots.len(), 1);
        assert_eq!(visible.sub_plots[0].id, IndicatorId::Macd);
        assert!(visible.sub_plots[0].value_range.lowest < 0.0);
        assert!(
            visible.candles.iter().all(|candle| candle.volume.is_none()),
            "the MACD box must not receive volume bars"
        );
    }

    /// VOL is the default sub plot: it prepares one box and its bars.
    #[test]
    fn the_default_volume_sub_plot_should_prepare_its_bars() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0))),
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 1,
            },
        );
        let indicators: Arc<[IndicatorSeries]> = Arc::from(Vec::new());

        let prepared = prepare_visible_chart(
            &snapshot,
            ChartViewport::default(),
            TimeFrame::Minute1,
            UtcOffset::UTC,
            layout,
            indicators,
            &[IndicatorId::Vol],
        );
        let visible = prepared.visible.expect("ten candles are visible");

        assert_eq!(visible.sub_plots.len(), 1);
        assert_eq!(visible.sub_plots[0].id, IndicatorId::Vol);
        assert_eq!(visible.sub_plots[0].value_range.lowest, 0.0);
        assert!(visible.sub_plots[0].value_range.highest > 0.0);
        assert!(
            visible.candles.iter().all(|candle| candle.volume.is_some()),
            "every fixture candle published a traded size"
        );
    }

    /// MACD spans are symmetric around zero and include the histogram, which is
    /// a line like the other two.
    #[test]
    fn sub_value_range_should_symmetrise_macd_and_include_the_histogram() {
        let series = test_series(
            IndicatorId::Macd,
            "MACD(12,26,9)",
            vec![
                test_line("DIF", vec![Some(-2.0), Some(0.5)], 0, LineShape::Line),
                test_line("DEA", vec![Some(1.0), Some(0.0)], 0, LineShape::Line),
                test_line("MACD", vec![Some(-8.0), Some(0.0)], 0, LineShape::Histogram),
            ],
            vec![0.0],
        );

        let value_range = sub_value_range(&series, 0..2);

        assert_eq!(value_range.lowest, -8.0);
        assert_eq!(value_range.highest, 8.0);
    }

    /// A MACD bar takes the colour of the direction it points: non-negative is
    /// bullish (a zero bar included) and negative is bearish. The colours are
    /// passed in so the test does not need a theme.
    #[test]
    fn macd_histogram_bars_should_use_the_direction_they_point() {
        let bullish = gpui_kit::white();
        let bearish = gpui_kit::black();

        assert_eq!(histogram_color(0.0, bullish, bearish), bullish);
        assert_eq!(histogram_color(1.0, bullish, bearish), bullish);
        assert_eq!(histogram_color(-1.0, bullish, bearish), bearish);
        assert_eq!(
            histogram_color(-f64::MIN_POSITIVE, bullish, bearish),
            bearish,
            "a tiny negative value is still negative"
        );
    }

    #[test]
    fn sub_value_range_should_keep_rsi_on_the_percentage_domain() {
        let series = test_series(
            IndicatorId::Rsi,
            "RSI",
            vec![test_line(
                "RSI6",
                vec![Some(12.0), Some(88.0)],
                0,
                LineShape::Line,
            )],
            vec![30.0, 70.0],
        );

        let value_range = sub_value_range(&series, 0..2);

        assert_eq!(value_range.lowest, 0.0);
        assert_eq!(value_range.highest, 100.0);
    }

    /// J can leave the 0..100 domain on either side, and the range has to follow
    /// it rather than clamp it at zero (design D11). The bounds are checked, not
    /// just their sign: a range that merely reaches just below zero would still
    /// cut the negative J off at the bottom of the box.
    #[test]
    fn sub_value_range_should_keep_a_negative_kdj_j_visible() {
        let series = test_series(
            IndicatorId::Kdj,
            "KDJ(9,3,3)",
            vec![
                test_line("K", vec![Some(50.0)], 0, LineShape::Line),
                test_line("D", vec![Some(50.0)], 0, LineShape::Line),
                test_line("J", vec![Some(-12.5)], 0, LineShape::Line),
            ],
            vec![20.0, 80.0],
        );

        let value_range = sub_value_range(&series, 0..1);

        // [-12.5, 100] padded by 5% of its span (5.625) on each side.
        assert!(
            (value_range.lowest - -18.125).abs() < 1.0e-9,
            "{value_range:?}"
        );
        assert!(
            (value_range.highest - 105.625).abs() < 1.0e-9,
            "{value_range:?}"
        );

        let above = test_series(
            IndicatorId::Kdj,
            "KDJ(9,3,3)",
            vec![
                test_line("K", vec![Some(50.0)], 0, LineShape::Line),
                test_line("D", vec![Some(50.0)], 0, LineShape::Line),
                test_line("J", vec![Some(150.0)], 0, LineShape::Line),
            ],
            vec![20.0, 80.0],
        );
        let value_range = sub_value_range(&above, 0..1);

        // [0, 150] padded by 5% of its span (7.5) on each side.
        assert!(
            (value_range.lowest - -7.5).abs() < 1.0e-9,
            "{value_range:?}"
        );
        assert!(
            (value_range.highest - 157.5).abs() < 1.0e-9,
            "{value_range:?}"
        );
    }

    /// All-None KDJ data falls back to the percentage domain rather than to an
    /// empty range.
    #[test]
    fn sub_value_range_should_fall_back_to_the_domain_when_kdj_has_no_values() {
        let series = test_series(
            IndicatorId::Kdj,
            "KDJ(9,3,3)",
            vec![
                test_line("K", vec![None, None], 0, LineShape::Line),
                test_line("D", vec![None, None], 0, LineShape::Line),
                test_line("J", vec![None, None], 0, LineShape::Line),
            ],
            vec![20.0, 80.0],
        );

        let value_range = sub_value_range(&series, 0..2);

        assert_eq!(value_range.lowest, 0.0);
        assert_eq!(value_range.highest, 100.0);
    }

    /// The range is derived from the visible window, so panning to a calmer or
    /// wilder stretch changes it even though the values did not move.
    #[test]
    fn sub_value_range_should_follow_the_visible_window() {
        let series = test_series(
            IndicatorId::Macd,
            "MACD(12,26,9)",
            vec![test_line(
                "DIF",
                vec![Some(0.1), Some(10.0)],
                0,
                LineShape::Line,
            )],
            vec![0.0],
        );

        assert_eq!(sub_value_range(&series, 0..1).highest, 0.1);
        assert_eq!(sub_value_range(&series, 1..2).highest, 10.0);
    }

    #[test]
    fn macd_axis_should_label_both_extremes_and_zero() {
        let value_range = ValueRange {
            lowest: -5.0,
            highest: 5.0,
        };

        assert_eq!(
            sub_axis_values(IndicatorId::Macd, value_range, px(100.0)),
            [-5.0, 0.0, 5.0]
        );
    }

    /// An all-zero MACD collapses to a 1e-9 span; the axis must still produce
    /// its three labels instead of panicking on a zero range.
    #[test]
    fn macd_axis_should_not_panic_on_an_all_zero_series() {
        let series = test_series(
            IndicatorId::Macd,
            "MACD(12,26,9)",
            vec![test_line(
                "DIF",
                vec![Some(0.0), Some(0.0)],
                0,
                LineShape::Line,
            )],
            vec![0.0],
        );
        let value_range = sub_value_range(&series, 0..2);

        assert_eq!(
            sub_axis_values(IndicatorId::Macd, value_range, px(100.0)).len(),
            3
        );
    }

    #[test]
    fn rsi_and_kdj_axes_should_keep_three_to_five_labels() {
        let domain = ValueRange {
            lowest: 0.0,
            highest: 100.0,
        };
        assert_eq!(
            sub_axis_values(IndicatorId::Rsi, domain, px(40.0)),
            [30.0, 50.0, 70.0]
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Rsi, domain, px(120.0)),
            [0.0, 30.0, 50.0, 70.0, 100.0]
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Kdj, domain, px(40.0)),
            [0.0, 50.0, 100.0]
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Kdj, domain, px(120.0)),
            [0.0, 20.0, 80.0, 100.0]
        );
    }

    /// The first-round regression filled a guide's dashed line with a solid grid
    /// line. The fix has to keep the guide labelled *and* keep its grid line out;
    /// both halves are pinned here, together with the line style the guide pass
    /// draws.
    #[test]
    fn sub_plot_axis_should_label_guides_without_drawing_their_grid_line() {
        let domain = ValueRange {
            lowest: 0.0,
            highest: 100.0,
        };

        let tall_rsi = sub_plot_axis(IndicatorId::Rsi, domain, px(120.0), &[30.0, 70.0]);
        assert_eq!(tall_rsi.labels, [0.0, 30.0, 50.0, 70.0, 100.0]);
        assert_eq!(tall_rsi.grid, [0.0, 50.0, 100.0]);
        assert_eq!(
            tall_rsi.guides,
            [
                SubPlotGuide {
                    value: 30.0,
                    solid: false,
                },
                SubPlotGuide {
                    value: 70.0,
                    solid: false,
                },
            ]
        );

        // The compact axis labels only 30/50/70, but 30 and 70 are still guides,
        // so only 50 may get a solid grid line.
        let compact_rsi = sub_plot_axis(IndicatorId::Rsi, domain, px(40.0), &[30.0, 70.0]);
        assert_eq!(compact_rsi.labels, [30.0, 50.0, 70.0]);
        assert_eq!(compact_rsi.grid, [50.0]);

        let tall_kdj = sub_plot_axis(IndicatorId::Kdj, domain, px(120.0), &[20.0, 80.0]);
        assert_eq!(tall_kdj.labels, [0.0, 20.0, 80.0, 100.0]);
        assert_eq!(tall_kdj.grid, [0.0, 100.0]);

        let macd = sub_plot_axis(
            IndicatorId::Macd,
            ValueRange {
                lowest: -5.0,
                highest: 5.0,
            },
            px(120.0),
            &[0.0],
        );
        assert_eq!(macd.labels, [-5.0, 0.0, 5.0]);
        assert_eq!(
            macd.grid,
            [-5.0, 5.0],
            "MACD's zero is a guide, not a grid line"
        );
        assert_eq!(
            macd.guides,
            [SubPlotGuide {
                value: 0.0,
                solid: true,
            }],
            "MACD's zero guide is solid, unlike the dashed percentage guides"
        );
    }

    /// Five labels need five 14px rows: the tall mode must still be used at
    /// exactly 70px and drop to three just below that.
    #[test]
    fn sub_plot_axis_should_switch_mode_at_five_label_rows() {
        let domain = ValueRange {
            lowest: 0.0,
            highest: 100.0,
        };

        assert_eq!(
            sub_axis_values(IndicatorId::Rsi, domain, px(70.0)).len(),
            5,
            "five 14px label rows fit exactly in 70px"
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Rsi, domain, px(69.9)).len(),
            3,
            "one pixel less cannot fit the five label rows"
        );
    }

    /// A KDJ range that leaves the domain on both sides has six candidates; the
    /// fixed endpoint closer to its extreme is dropped so at most five remain.
    #[test]
    fn kdj_axis_should_drop_the_fixed_endpoint_nearest_the_dynamic_extreme() {
        let value_range = ValueRange {
            lowest: -18.0,
            highest: 124.0,
        };

        let values = sub_axis_values(IndicatorId::Kdj, value_range, px(120.0));

        assert!((3..=5).contains(&values.len()), "got {values:?}");
        assert_eq!(values, [-18.0, 20.0, 80.0, 100.0, 124.0]);
    }

    /// An in-domain series still gets the 5% padding, but the padding is not a
    /// dynamic extremum: labelling it would stack `-5` / `105` on top of `0` /
    /// `100` and push the fixed labels out of the axis.
    #[test]
    fn kdj_axis_should_not_label_the_five_percent_padding() {
        let series = test_series(
            IndicatorId::Kdj,
            "KDJ(9,3,3)",
            vec![
                test_line("K", vec![Some(50.0)], 0, LineShape::Line),
                test_line("D", vec![Some(50.0)], 0, LineShape::Line),
                test_line("J", vec![Some(60.0)], 0, LineShape::Line),
            ],
            vec![20.0, 80.0],
        );
        let value_range = sub_value_range(&series, 0..1);

        assert!(
            value_range.lowest < 0.0 && value_range.highest > 100.0,
            "the range is padded beyond the domain: {value_range:?}"
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Kdj, value_range, px(120.0)),
            [0.0, 20.0, 80.0, 100.0]
        );
    }

    /// The signed MACD tick has to be able to widen the shared axis; the
    /// candidate set is where that width comes from.
    #[test]
    fn axis_label_candidates_should_include_signed_macd_ticks() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];
        let tick = test_tick();
        let price = Price::from_str("123.45").expect("price should parse");
        let inner = price.inner() as f64;
        let series = [test_series(
            IndicatorId::Macd,
            "MACD(12,26,9)",
            vec![test_line("DIF", vec![Some(inner)], 0, LineShape::Line)],
            vec![0.0],
        )];

        let labels = axis_label_candidates(
            &candles,
            0..1,
            None,
            tick,
            px(300.0),
            &series,
            &[IndicatorId::Macd],
        );

        assert!(
            labels.contains(&format_signed_price(-inner, tick)),
            "negative MACD candidates were {labels:?}"
        );
        assert!(labels.contains(&format_signed_price(inner, tick)));
        let baseline = axis_label_candidates(&candles, 0..1, None, tick, px(300.0), &[], &[]);
        assert!(
            !baseline.contains(&format_signed_price(-inner, tick)),
            "without the MACD sub plot the signed tick is not a candidate"
        );
    }

    /// The percentage reference lines are axis labels too: the candidate set
    /// feeds the shared axis width, so it must contain the 30/70 and 20/80 ticks
    /// even though their solid grid lines are skipped.
    #[test]
    fn axis_label_candidates_should_include_percentage_guide_ticks() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];
        let tick = test_tick();
        let series = [
            test_series(
                IndicatorId::Rsi,
                "RSI",
                vec![test_line("RSI6", vec![Some(48.0)], 0, LineShape::Line)],
                vec![30.0, 70.0],
            ),
            test_series(
                IndicatorId::Kdj,
                "KDJ(9,3,3)",
                vec![test_line("J", vec![Some(58.0)], 0, LineShape::Line)],
                vec![20.0, 80.0],
            ),
        ];

        let labels = axis_label_candidates(
            &candles,
            0..1,
            None,
            tick,
            px(300.0),
            &series,
            &[IndicatorId::Rsi, IndicatorId::Kdj],
        );
        for value in [0.0, 30.0, 50.0, 70.0, 100.0, 20.0, 80.0] {
            assert!(
                labels.contains(&format_percentage_tick(value)),
                "the tick {value} is missing from {labels:?}"
            );
        }

        let baseline = axis_label_candidates(&candles, 0..1, None, tick, px(300.0), &[], &[]);
        assert!(
            !baseline.contains(&format_percentage_tick(30.0)),
            "without an RSI sub plot the 30 guide is not a candidate"
        );
    }

    /// VOL's legend reads the snapshot's traded size at the absolute candle.
    #[test]
    fn the_volume_legend_should_read_the_visible_candle() {
        let snapshot = snapshot_of([candle_with_volume(1, Quantity::from(1_234_u64))]);
        let entries = sub_plot_legend(
            IndicatorId::Vol,
            &[],
            0,
            &snapshot,
            test_tick(),
            gpui_kit::white(),
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "VOL 1.234K");
    }

    /// The sub-plot legend indexes the values by absolute candle, and only shows
    /// a line once that line is readable there.
    #[test]
    fn the_macd_legend_should_read_the_absolute_candle_past_readable_from() {
        let snapshot = snapshot_with_candles(20);
        let price = Price::from_str("0.52").expect("price should parse");
        let mut values = vec![None; 20];
        values[14] = Some(price.inner() as f64);
        values[15] = Some(price.inner() as f64);
        let series = [test_series(
            IndicatorId::Macd,
            "MACD(12,26,9)",
            vec![test_line("DIF", values, 15, LineShape::Line)],
            vec![0.0],
        )];
        let tick = test_tick();
        let axis_color = gpui_kit::white();

        let before = sub_plot_legend(IndicatorId::Macd, &series, 14, &snapshot, tick, axis_color);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].text, "MACD(12,26,9)");

        let after = sub_plot_legend(IndicatorId::Macd, &series, 15, &snapshot, tick, axis_color);
        assert_eq!(after.len(), 2);
        assert_eq!(after[1].text, "DIF 0.52");
    }

    /// RSI's line labels already carry the formula, so the legend must not
    /// repeat it as `RSI RSI6`; KDJ reads its three lines in percent.
    #[test]
    fn percentage_sub_plot_legends_should_not_repeat_the_formula() {
        let snapshot = snapshot_with_candles(1);
        let tick = test_tick();
        let axis_color = gpui_kit::white();
        let rsi = [test_series(
            IndicatorId::Rsi,
            "RSI",
            vec![test_line("RSI6", vec![Some(48.26)], 0, LineShape::Line)],
            vec![30.0, 70.0],
        )];
        let entries = sub_plot_legend(IndicatorId::Rsi, &rsi, 0, &snapshot, tick, axis_color);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "RSI6 48.3");

        let kdj = [test_series(
            IndicatorId::Kdj,
            "KDJ(9,3,3)",
            vec![
                test_line("K", vec![Some(52.14)], 0, LineShape::Line),
                test_line("D", vec![Some(48.66)], 0, LineShape::Line),
                test_line("J", vec![Some(58.9)], 0, LineShape::Line),
            ],
            vec![20.0, 80.0],
        )];
        let entries = sub_plot_legend(IndicatorId::Kdj, &kdj, 0, &snapshot, tick, axis_color);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["KDJ(9,3,3)", "K 52.1", "D 48.7", "J 58.9"]
        );
    }

    /// A J that is a hair below zero has no meaningful negative magnitude, but
    /// `{:.1}` would print it as `-0.0`; the readout must round to `0.0`.
    #[test]
    fn a_tiny_negative_percentage_should_not_read_as_negative_zero() {
        assert_eq!(format_percentage_value(0.0), "0.0");
        assert_eq!(format_percentage_value(-0.04), "0.0");
        assert_eq!(format_percentage_value(-0.06), "-0.1");
        assert_eq!(format_percentage_value(48.26), "48.3");
        assert_eq!(format_percentage_tick(-0.0), "0");
        assert_eq!(format_percentage_tick(0.0), "0");
    }

    fn one_candle_snapshot() -> CandlestickSeriesSnapshot {
        let mut series = crate::model::CandlestickSeries::new(
            10,
            ListingPrecision {
                price_tick: Tick::from_str("0.01").expect("price tick should parse"),
                quantity_tick: Tick::from_str("0.001").expect("quantity step should parse"),
            },
        );
        series.merge_initial([candle(1)]);
        series.snapshot()
    }
}
