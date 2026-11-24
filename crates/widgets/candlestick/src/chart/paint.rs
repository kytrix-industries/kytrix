use std::sync::Arc;

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::{
    catalog::display::catalog::DisplaySnapshot, market::timeframe::TimeFrame,
};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Quantity, Tick},
    market::{book::ListingCandlestick, units::QuantityUnit},
};
use smol_str::{SmolStr, format_smolstr};
use time::UtcOffset;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::{ClipRect, Path, Quad, QuadPaint},
        style::StylePatch,
        style_builder::StyleBuilder,
    },
    text::TextStyle,
};
use ui_kit::theme::{components::ChartColors, resolved::Theme, role::Role};
use widget::cx::WidgetCx;

use super::{
    geometry::{
        CandleGeometry, ChartLayout, ChartViewport, LayoutInputs, SubPlotLayout, visible_range,
    },
    text::{
        AXIS_FONT_SIZE, ChartTextCache, ReadoutCache, ReadoutPresentation, ReadoutSpec,
        TEXT_ROW_HEIGHT, candle_color, format_price, format_signed_price, format_volume, style_of,
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

const SELECTOR: &str = "CandlestickChart";

const AXIS_TICK_TEXT_INSET: LogicalPx = px(10.0);
const AXIS_TAG_TEXT_INSET: LogicalPx = px(8.0);
const LATEST_PRICE_TAG_HEIGHT: LogicalPx = px(38.0);
const LATEST_PRICE_TAG_PRICE_HEIGHT: LogicalPx = px(20.0);
const LATEST_PRICE_MAX_CANDLE_GAP: usize = 40;
const PRICE_AXIS_TARGET_LABEL_SPACING: LogicalPx = px(44.0);
const PRICE_AXIS_MIN_LABEL_SPACING: LogicalPx = px(28.0);
const MAX_PRICE_AXIS_TARGET_INTERVALS: f64 = 16.0;
const MAX_PRICE_AXIS_TICKS: usize = 24;
const TEXT_ROW_INSET: LogicalPx = px(4.0);
const FIRST_TEXT_ROW_TOP: LogicalPx = TEXT_ROW_INSET;
const READOUT_LEGEND_GAP: LogicalPx = px(2.0);
const MAIN_LEGEND_TOP: LogicalPx =
    px(TEXT_ROW_INSET.value() + TEXT_ROW_HEIGHT.value() + READOUT_LEGEND_GAP.value());
const VOLUME_HEADROOM: f64 = 1.08;
const SUB_PLOT_COMPACT_HEIGHT: LogicalPx = px(5.0 * TEXT_ROW_HEIGHT.value());
const CROSSHAIR_TAG_HEIGHT: LogicalPx = px(20.0);
const CROSSHAIR_TIME_TAG_WIDTH: LogicalPx = px(132.0);
const DASH_LENGTH: LogicalPx = px(5.0);
const DASH_GAP: LogicalPx = px(9.0);
const LINE_WIDTH: LogicalPx = px(1.0);
const LEGEND_ENTRY_GAP: LogicalPx = px(10.0);

pub(crate) struct ChartFrame<'a> {
    pub snapshot: &'a CandlestickSeriesSnapshot,
    pub viewport: ChartViewport,
    pub interval: TimeFrame,
    pub countdown_label: &'a SmolStr,
    pub indicators: &'a CandlestickIndicatorConfig,
    pub panel_open: bool,
    pub data_time_zone: UtcOffset,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ChartPalette {
    chart: ChartColors,
    bullish_weak: Color,
    bearish_weak: Color,
    tag_background: Color,
    tag_foreground: Color,
    muted_text: Color,
}

impl ChartPalette {
    fn resolve(theme: &Theme) -> Self {
        Self {
            chart: theme.components().chart,
            bullish_weak: theme.role(Role::MarketUpWeak),
            bearish_weak: theme.role(Role::MarketDownWeak),
            tag_background: theme.role(Role::BrandBase),
            tag_foreground: theme.role(Role::FgOnBrand),
            muted_text: theme.role(Role::FgMuted),
        }
    }
}

pub(crate) struct ChartCanvas {
    node: Option<NodeId>,
    bounds: Option<LogicalRect>,
    layout: Option<ChartLayout>,
    painted: Option<PaintedCanvas>,
    text: ChartTextCache,
    readout: ReadoutCache,
    indicators: Option<IndicatorValues>,
    #[cfg(test)]
    indicator_builds: usize,
    geometry: Vec<GeometryId>,
}

struct PaintedCanvas {
    key: PaintedKey,
    snapshot: CandlestickSeriesSnapshot,
    indicators: CandlestickIndicatorConfig,
    countdown_label: SmolStr,
}

impl PaintedCanvas {
    fn matches(&self, frame: &ChartFrame<'_>, key: PaintedKey, text_is_stale: bool) -> bool {
        !text_is_stale
            && self.key == key
            && self.snapshot.shares_chart_data_with(frame.snapshot)
            && self.indicators == *frame.indicators
            && self.countdown_label == *frame.countdown_label
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    bounds: LogicalRect,
    candle_width: LogicalPx,
    right_timestamp: Option<i64>,
    cursor: Option<LogicalPoint>,
    interval: TimeFrame,
    zone: UtcOffset,
    panel_open: bool,
    colors: ChartPalette,
    language: LanguageCode,
    text_scale: f32,
    catalog_generation: u64,
}

struct IndicatorValues {
    snapshot: CandlestickSeriesSnapshot,
    config: CandlestickIndicatorConfig,
    series: Arc<[IndicatorSeries]>,
}

impl ChartCanvas {
    pub(crate) fn new() -> Self {
        Self {
            node: None,
            bounds: None,
            layout: None,
            painted: None,
            text: ChartTextCache::new(),
            readout: ReadoutCache::default(),
            indicators: None,
            #[cfg(test)]
            indicator_builds: 0,
            geometry: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    pub(crate) fn bounds(&self) -> Option<LogicalRect> {
        self.bounds
    }

    pub(crate) fn layout(&self) -> Option<&ChartLayout> {
        self.layout.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn layout_width(&self) -> Option<LogicalPx> {
        self.layout
            .as_ref()
            .map(|layout| layout.price_plot.size.width)
    }

    #[cfg(test)]
    pub(crate) fn selector() -> &'static str {
        SELECTOR
    }

    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let label = t_smol!(translate, "widget.candlestick.chart");
        let node = NodeBuilder::canvas(Canvas::new(LogicalRect::default()))
            .id(SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .tab_stop(false)
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                label: Some(label),
                ..A11yOverride::default()
            })
            .bind_a11y(move |read| {
                let _language = read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(translate, "widget.candlestick.chart")),
                    ..A11yPatch::default()
                }
            })
            .bind_style(move |read| {
                let _appearance = theme.appearance(read);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        self.node = Some(node);
        Ok(())
    }

    pub(crate) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        frame: &ChartFrame<'_>,
        display: Option<&DisplaySnapshot>,
    ) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = target.layout().bounds;
        if bounds.size.is_empty() {
            return Ok(false);
        }
        self.bounds = Some(bounds);

        let appearance = cx.theme().current(cx.app());
        let colors = ChartPalette::resolve(cx.theme().theme_for(appearance));
        let language = *cx.read(&cx.language());
        let text_scale = cx.app().text_scale();
        let translate = cx.translate().clone();
        let key = PaintedKey {
            bounds,
            candle_width: frame.viewport.candle_width,
            right_timestamp: frame.viewport.right_timestamp,
            cursor: frame.viewport.cursor,
            interval: frame.interval,
            zone: frame.data_time_zone,
            panel_open: frame.panel_open,
            colors,
            language,
            text_scale,
            catalog_generation: display
                .as_ref()
                .map_or(0, |display| display.generation_id().get()),
        };
        let text_is_stale = self.text.has_stale() || self.readout.has_stale();
        if self
            .painted
            .as_ref()
            .is_some_and(|painted| painted.matches(frame, key, text_is_stale))
        {
            return Ok(false);
        }

        let (canvas, geometry) =
            self.paint(cx.app_mut(), &translate, display, frame, bounds, colors)?;
        let changed = cx.set_canvas(node, canvas)?;
        if changed {
            for id in std::mem::take(&mut self.geometry) {
                cx.app_mut().geometry_mut().release(id);
            }
            self.geometry = geometry;
            self.painted = Some(PaintedCanvas {
                key,
                snapshot: frame.snapshot.clone(),
                indicators: frame.indicators.clone(),
                countdown_label: frame.countdown_label.clone(),
            });
        } else {
            for id in geometry {
                cx.app_mut().geometry_mut().release(id);
            }
        }
        Ok(changed)
    }

    pub(crate) fn clear(&mut self) {
        self.layout = None;
        self.painted = None;
        self.indicators = None;
        self.text.clear();
        self.readout.clear();
    }

    pub(crate) fn release(&mut self, cx: &mut WidgetCx<'_>) {
        for id in std::mem::take(&mut self.geometry) {
            cx.app_mut().geometry_mut().release(id);
        }
        if let Some(node) = self.node.take()
            && cx.tree().get(node).is_ok()
        {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        self.clear();
        self.bounds = None;
    }

    fn series(
        &mut self,
        snapshot: &CandlestickSeriesSnapshot,
        config: &CandlestickIndicatorConfig,
    ) -> Arc<[IndicatorSeries]> {
        if let Some(cached) = &self.indicators
            && cached.snapshot.shares_chart_data_with(snapshot)
            && cached.config == *config
        {
            return Arc::clone(&cached.series);
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
        self.indicators = Some(IndicatorValues {
            snapshot: snapshot.clone(),
            config: config.clone(),
            series: Arc::clone(&series),
        });
        #[cfg(test)]
        {
            self.indicator_builds += 1;
        }
        series
    }

    #[cfg(test)]
    pub(crate) fn indicator_builds(&self) -> usize {
        self.indicator_builds
    }

    fn paint(
        &mut self,
        app: &mut App,
        translate: &Translate,
        display: Option<&DisplaySnapshot>,
        frame: &ChartFrame<'_>,
        bounds: LogicalRect,
        colors: ChartPalette,
    ) -> Result<(Canvas, Vec<GeometryId>), EngineError> {
        self.text.drop_stale();
        self.readout.drop_stale();
        let series = self.series(frame.snapshot, frame.indicators);
        let sub_plot_ids = enabled_sub_plot_ids(frame.indicators);
        let layout = resolve_chart_layout(
            app,
            &mut self.text,
            frame,
            bounds,
            colors,
            &series,
            &sub_plot_ids,
        );
        let visible = prepare_visible_chart(frame, &layout, &series, &sub_plot_ids);
        self.layout = Some(layout);
        let Some(layout) = self.layout.as_ref() else {
            return Ok((Canvas::new(bounds), Vec::new()));
        };

        let mut painter = ChartPainter {
            app,
            text: &mut self.text,
            readout: &mut self.readout,
            translate,
            display,
            frame,
            layout,
            visible: visible.as_ref(),
            series: &series,
            colors,
            bounds,
            canvas: Canvas::new(bounds),
            geometry: Vec::new(),
            price_tick: frame.snapshot.price_tick(),
        };
        painter.paint()?;
        Ok((painter.canvas, painter.geometry))
    }
}

struct PreparedAxisTick {
    value: f64,
    label: SmolStr,
}

struct PreparedTimeTick {
    x: LogicalPx,
    label: SmolStr,
}

struct PreparedLatestPrice {
    y: LogicalPx,
    label: SmolStr,
    is_up: bool,
}

struct PreparedCandle {
    wick: LogicalRect,
    body: LogicalRect,
    volume: Option<LogicalRect>,
    is_up: bool,
}

struct PreparedSubPlot {
    id: IndicatorId,
    value_range: ValueRange,
}

struct VisibleChart {
    range: std::ops::Range<usize>,
    lowest: f64,
    highest: f64,
    geometry: CandleGeometry,
    candles: Vec<PreparedCandle>,
    price_ticks: Vec<PreparedAxisTick>,
    time_ticks: Vec<PreparedTimeTick>,
    latest_price: Option<PreparedLatestPrice>,
    sub_plots: Vec<PreparedSubPlot>,
}

fn resolve_chart_layout(
    app: &mut App,
    text: &mut ChartTextCache,
    frame: &ChartFrame<'_>,
    bounds: LogicalRect,
    colors: ChartPalette,
    series: &[IndicatorSeries],
    sub_plot_ids: &[IndicatorId],
) -> ChartLayout {
    let provisional = ChartLayout::new(
        bounds,
        LayoutInputs {
            longest_label_width: LogicalPx::ZERO,
            sub_plot_count: sub_plot_ids.len(),
        },
    );
    let range = visible_range(
        frame.snapshot.candles(),
        frame.viewport,
        provisional.price_plot.size.width,
    );
    let latest = latest_candle_for_indicator(frame.snapshot, range.end);
    let style = style_of(AXIS_FONT_SIZE, colors.chart.axis);
    let longest_label_width = axis_label_candidates(
        frame.snapshot.candles(),
        range,
        latest.map(|candle| CandlePrices::of(candle).close()),
        frame.snapshot.price_tick(),
        provisional.price_plot.size.height,
        series,
        sub_plot_ids,
    )
    .into_iter()
    .map(|label| text.width(app, &label, &style))
    .fold(LogicalPx::ZERO, LogicalPx::max);
    let countdown_width = latest.map_or(LogicalPx::ZERO, |_| {
        text.width(app, maximum_countdown_label(frame.interval), &style)
    });
    ChartLayout::new(
        bounds,
        LayoutInputs {
            longest_label_width: longest_label_width.max(countdown_width),
            sub_plot_count: sub_plot_ids.len(),
        },
    )
}

fn axis_label_candidates(
    candles: &[ListingCandlestick],
    range: std::ops::Range<usize>,
    latest_price: Option<Price>,
    price_tick: Tick,
    price_plot_height: LogicalPx,
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

fn axis_text_width(axis_width: LogicalPx, inset: LogicalPx) -> LogicalPx {
    (axis_width - inset).max(LogicalPx::ZERO)
}

fn axis_label_clip(axis: LogicalRect) -> LogicalRect {
    LogicalRect::new(
        axis.origin.x,
        axis.origin.y,
        axis_text_width(axis.size.width, AXIS_TICK_TEXT_INSET),
        axis.size.height,
    )
}

fn prepare_visible_chart(
    frame: &ChartFrame<'_>,
    layout: &ChartLayout,
    indicators: &[IndicatorSeries],
    sub_plot_ids: &[IndicatorId],
) -> Option<VisibleChart> {
    let range = visible_range(
        frame.snapshot.candles(),
        frame.viewport,
        layout.price_plot.size.width,
    );
    if range.is_empty() {
        return None;
    }
    let volume_plot = if sub_plot_ids.first() == Some(&IndicatorId::Vol) {
        layout.sub_plots.first().map(|sub_plot| sub_plot.plot)
    } else {
        None
    };
    let candles = frame.snapshot.candles();
    let latest = latest_candle_for_indicator(frame.snapshot, range.end);
    let visible = &candles[range.clone()];
    let (lowest, highest) = price_bounds_with_indicators(
        candles,
        range.clone(),
        latest.map(|candle| CandlePrices::of(candle).close()),
        indicators,
    );
    let volume_upper = volume_upper_bound(visible);
    let sub_plots: Vec<PreparedSubPlot> = if layout.sub_plots.is_empty() {
        Vec::new()
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
                value_range: sub_plot_value_range(*id, volume_upper, range.clone(), indicators),
            })
            .collect()
    };
    let geometry = CandleGeometry::new(layout.price_plot, range.len(), frame.viewport.candle_width);
    let body_width = geometry.body_width();
    let prepared_candles: Vec<PreparedCandle> = visible
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
        .collect();
    let price_tick = frame.snapshot.price_tick();
    let price_ticks = price_axis_values(lowest, highest, layout.price_plot.size.height, price_tick)
        .into_iter()
        .map(|value| PreparedAxisTick {
            value,
            label: format_price(value, price_tick),
        })
        .collect::<Vec<_>>();
    let label_stride = ((px(70.0) / frame.viewport.candle_width).ceil() as usize).max(1);
    let first_tick = range.start.div_ceil(label_stride) * label_stride;
    let time_ticks = (first_tick..range.end)
        .step_by(label_stride)
        .map(|index| PreparedTimeTick {
            x: geometry.center_x(index - range.start),
            label: format_axis_time(
                candles[index].timestamp,
                frame.interval,
                frame.data_time_zone,
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
            label: prices.close().format_and_round_tick(price_tick),
            is_up: prices.is_up(),
        }
    });
    Some(VisibleChart {
        range,
        lowest,
        highest,
        geometry,
        candles: prepared_candles,
        price_ticks,
        time_ticks,
        latest_price,
        sub_plots,
    })
}

#[derive(Clone, Copy)]
struct BarColors {
    up: Color,
    down: Color,
    up_resting: Color,
    down_resting: Color,
}

impl BarColors {
    fn of(colors: ChartPalette) -> Self {
        Self {
            up: colors.chart.bullish,
            down: colors.chart.bearish,
            up_resting: colors.bullish_weak,
            down_resting: colors.bearish_weak,
        }
    }

    fn bar(self, up: bool, hovered: bool) -> Color {
        match (up, hovered) {
            (true, true) => self.up,
            (true, false) => self.up_resting,
            (false, true) => self.down,
            (false, false) => self.down_resting,
        }
    }
}

fn histogram_bar_color(value: f64, hovered: bool, bars: BarColors) -> Color {
    bars.bar(value >= 0.0, hovered)
}

struct LegendEntry {
    text: SmolStr,
    color: Color,
}

fn main_indicator_legend(
    indicators: &[IndicatorSeries],
    absolute: usize,
    price_tick: Tick,
) -> Vec<Vec<LegendEntry>> {
    let mut rows = Vec::new();
    for series in indicators {
        if definition_of(series.id).pane != IndicatorPane::Main {
            continue;
        }
        let mut row = Vec::new();
        for line in &series.lines {
            if absolute < line.readable_from {
                continue;
            }
            let Some(value) = line.values.get(absolute).copied().flatten() else {
                continue;
            };
            row.push(LegendEntry {
                text: format_smolstr!("{} {}", line.label, format_price(value, price_tick)),
                color: line.color,
            });
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    rows
}

fn sub_plot_legend(
    id: IndicatorId,
    indicators: &[IndicatorSeries],
    absolute: usize,
    snapshot: &CandlestickSeriesSnapshot,
    price_tick: Tick,
    axis_color: Color,
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

fn readout_candle_index(
    cursor: Option<LogicalPoint>,
    layout: &ChartLayout,
    visible: &VisibleChart,
) -> usize {
    debug_assert!(
        !visible.range.is_empty(),
        "a visible chart always has at least one candle in range: {:?}",
        visible.range
    );
    let slot = cursor
        .filter(|cursor| layout.contains_data_point(*cursor))
        .and_then(|cursor| visible.geometry.slot_at(cursor.x))
        .unwrap_or(visible.range.len() - 1);
    visible.range.start + slot
}

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

pub(crate) fn latest_price_indicator_is_visible(
    candle_count: usize,
    visible_end: usize,
    at_live_edge: bool,
) -> bool {
    candle_count > 0
        && at_live_edge
        && candle_count.saturating_sub(visible_end) <= LATEST_PRICE_MAX_CANDLE_GAP
}

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

fn padded_price_bounds(lowest: f64, highest: f64) -> (f64, f64) {
    let range = (highest - lowest).max(1.0);
    (lowest - range * 0.04, highest + range * 0.04)
}

#[cfg(test)]
fn price_bounds(candles: &[ListingCandlestick], latest_price: Option<Price>) -> (f64, f64) {
    let (lowest, highest) = price_extremes(candles, latest_price);
    padded_price_bounds(lowest, highest)
}

fn price_bounds_with_indicators(
    candles: &[ListingCandlestick],
    range: std::ops::Range<usize>,
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

fn price_axis_values(
    lowest: f64,
    highest: f64,
    plot_height: LogicalPx,
    price_tick: Tick,
) -> Vec<f64> {
    let range = highest - lowest;
    if !range.is_finite() || range <= 0.0 {
        return vec![lowest];
    }

    let plot_height = plot_height.max(LogicalPx::ZERO).value();
    if plot_height < PRICE_AXIS_MIN_LABEL_SPACING.value() {
        return vec![(lowest + highest) / 2.0];
    }

    let plot_height = f64::from(plot_height);
    let target_intervals = (plot_height / f64::from(PRICE_AXIS_TARGET_LABEL_SPACING.value()))
        .round()
        .clamp(1.0, MAX_PRICE_AXIS_TARGET_INTERVALS);
    let preferred_step = nearest_nice_step(range / target_intervals);
    let minimum_step =
        nice_step(range * f64::from(PRICE_AXIS_MIN_LABEL_SPACING.value()) / plot_height);
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

fn sub_plot_value_range(
    id: IndicatorId,
    volume_upper: f64,
    range: std::ops::Range<usize>,
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

fn sub_value_range(series: &IndicatorSeries, range: std::ops::Range<usize>) -> ValueRange {
    debug_assert!(range.start <= range.end);
    let Some(first_line) = series.lines.first() else {
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
            let padding = (highest - lowest) * 0.05;
            ValueRange {
                lowest: lowest - padding,
                highest: highest + padding,
            }
        }
        IndicatorId::Ma | IndicatorId::Ema | IndicatorId::Vol => ValueRange {
            lowest: 0.0,
            highest: 100.0,
        },
    }
}

fn sub_axis_values(id: IndicatorId, value_range: ValueRange, plot_height: LogicalPx) -> Vec<f64> {
    let compact = plot_height < SUB_PLOT_COMPACT_HEIGHT;
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
        IndicatorId::Ma | IndicatorId::Ema => vec![value_range.lowest, value_range.highest],
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SubPlotGuide {
    value: f64,
    solid: bool,
}

#[derive(Debug, PartialEq)]
struct SubPlotAxis {
    labels: Vec<f64>,
    grid: Vec<f64>,
    guides: Vec<SubPlotGuide>,
}

fn sub_plot_axis(
    id: IndicatorId,
    value_range: ValueRange,
    plot_height: LogicalPx,
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

fn format_percentage_tick(value: f64) -> SmolStr {
    if value == 0.0 {
        SmolStr::new_static("0")
    } else {
        format_smolstr!("{value:.0}")
    }
}

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

fn y_for_sub_value(value: f64, value_range: ValueRange, plot: LogicalRect) -> LogicalPx {
    let span = (value_range.highest - value_range.lowest).max(f64::MIN_POSITIVE);
    let ratio = ((value_range.highest - value) / span).clamp(0.0, 1.0);
    plot.origin.y + plot.size.height * ratio as f32
}

fn sub_value_from_y(y: LogicalPx, value_range: ValueRange, plot: LogicalRect) -> f64 {
    let ratio = if plot.size.height > px(0.0) {
        f64::from((y - plot.origin.y) / plot.size.height)
    } else {
        0.0
    };
    value_range.highest - (value_range.highest - value_range.lowest) * ratio.clamp(0.0, 1.0)
}

fn candle_wick_bounds(
    prices: CandlePrices,
    center_x: LogicalPx,
    lowest: f64,
    highest: f64,
    plot: LogicalRect,
) -> LogicalRect {
    let high_y = y_for_price(prices.high().inner() as f64, lowest, highest, plot);
    let low_y = y_for_price(prices.low().inner() as f64, lowest, highest, plot);
    LogicalRect::new(
        center_x - LINE_WIDTH / 2.0,
        high_y,
        LINE_WIDTH,
        (low_y - high_y).max(LINE_WIDTH),
    )
}

fn candle_body_bounds(
    prices: CandlePrices,
    center_x: LogicalPx,
    body_width: LogicalPx,
    lowest: f64,
    highest: f64,
    plot: LogicalRect,
) -> LogicalRect {
    let open_y = y_for_price(prices.open().inner() as f64, lowest, highest, plot);
    let close_y = y_for_price(prices.close().inner() as f64, lowest, highest, plot);
    LogicalRect::new(
        center_x - body_width / 2.0,
        open_y.min(close_y),
        body_width,
        (close_y - open_y).abs().max(px(1.0)),
    )
}

fn candle_volume_bounds(
    candle: &ListingCandlestick,
    center_x: LogicalPx,
    body_width: LogicalPx,
    upper: f64,
    plot: Option<LogicalRect>,
) -> Option<LogicalRect> {
    let plot = plot?;
    let quantity = candle.traded_quantity.as_ref()?;
    if quantity.value.is_zero() {
        return None;
    }
    let top = y_for_volume(quantity.value.inner() as f64, upper, plot);
    Some(LogicalRect::new(
        center_x - body_width / 2.0,
        top,
        body_width,
        (plot.max().y - top).max(px(1.0)),
    ))
}

fn traded_unit_label(
    candle: &ListingCandlestick,
    translate: &Translate,
    display: Option<&DisplaySnapshot>,
) -> SmolStr {
    match &candle.traded_quantity {
        Some(quantity) => unit_label(&quantity.unit, translate, display),
        None => SmolStr::default(),
    }
}

fn unit_label(
    unit: &QuantityUnit,
    translate: &Translate,
    display: Option<&DisplaySnapshot>,
) -> SmolStr {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_label(asset, translate, display)
        }
        QuantityUnit::Contracts => t_smol!(translate, "widget.candlestick.contracts"),
        QuantityUnit::OutcomeTokens { .. } => {
            t_smol!(translate, "widget.candlestick.outcome_tokens")
        }
    }
}

fn asset_label(
    asset: &AssetId,
    translate: &Translate,
    display: Option<&DisplaySnapshot>,
) -> SmolStr {
    display
        .and_then(|display| display.resolver().asset_code(asset).ok())
        .map_or_else(
            || t_smol!(translate, "error.display.unknown_asset"),
            |code| SmolStr::new(code.as_str()),
        )
}

fn for_each_horizontal_dash(y: LogicalPx, plot: LogicalRect, mut f: impl FnMut(LogicalRect)) {
    let right = plot.max().x;
    let mut x = plot.origin.x;
    while x < right {
        f(LogicalRect::new(
            x,
            y,
            DASH_LENGTH.min(right - x),
            LINE_WIDTH,
        ));
        x += DASH_GAP;
    }
}

fn for_each_vertical_dash(x: LogicalPx, plot: LogicalRect, mut f: impl FnMut(LogicalRect)) {
    let bottom = plot.max().y;
    let mut y = plot.origin.y;
    while y < bottom {
        f(LogicalRect::new(
            x,
            y,
            LINE_WIDTH,
            DASH_LENGTH.min(bottom - y),
        ));
        y += DASH_GAP;
    }
}

fn readout_bounds(price_plot: LogicalRect) -> LogicalRect {
    let top = FIRST_TEXT_ROW_TOP;
    LogicalRect::new(
        price_plot.origin.x + TEXT_ROW_INSET,
        price_plot.origin.y + top,
        (price_plot.size.width - TEXT_ROW_INSET * 2.0).max(px(0.0)),
        (price_plot.size.height - top - TEXT_ROW_INSET).clamp(px(0.0), TEXT_ROW_HEIGHT),
    )
}

fn y_for_price(price: f64, lowest: f64, highest: f64, plot: LogicalRect) -> LogicalPx {
    let ratio = ((highest - price) / (highest - lowest).max(1.0)).clamp(0.0, 1.0);
    plot.origin.y + plot.size.height * ratio as f32
}

fn price_for_y(y: LogicalPx, lowest: f64, highest: f64, plot: LogicalRect) -> f64 {
    let ratio = f64::from((y - plot.origin.y) / plot.size.height);
    highest - (highest - lowest) * ratio.clamp(0.0, 1.0)
}

fn y_for_volume(volume: f64, upper: f64, plot: LogicalRect) -> LogicalPx {
    let ratio = (volume / upper.max(1.0)).clamp(0.0, 1.0);
    plot.max().y - plot.size.height * ratio as f32
}

fn volume_for_y(y: LogicalPx, upper: f64, plot: LogicalRect) -> f64 {
    let ratio = f64::from((plot.max().y - y) / plot.size.height);
    upper * ratio.clamp(0.0, 1.0)
}

fn axis_label_y(y: LogicalPx, axis: LogicalRect) -> LogicalPx {
    let label_height = TEXT_ROW_HEIGHT.min(axis.size.height);
    (y - label_height / 2.0)
        .max(axis.origin.y)
        .min(axis.max().y - label_height)
}

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

struct LineMesh {
    vertex_bytes: Vec<u8>,
    index_bytes: Vec<u8>,
    bounds: LogicalRect,
}

impl LineMesh {
    fn polyline(
        values: &[Option<f64>],
        range: std::ops::Range<usize>,
        geometry: CandleGeometry,
        y_for: impl Fn(f64) -> LogicalPx,
    ) -> Option<Self> {
        debug_assert!(range.start <= range.end);
        debug_assert!(
            range.end <= values.len(),
            "the range indexes the whole snapshot, got {range:?} for {} values",
            values.len()
        );

        let mut mesh = MeshBuilder::default();
        let mut previous: Option<(LogicalPoint, bool)> = None;
        let first = range.start.saturating_sub(1);
        for (index, value) in values.iter().enumerate().take(range.end).skip(first) {
            let Some(value) = value else {
                previous = None;
                continue;
            };
            let x = if index < range.start {
                geometry.center_x_left_of_first(range.start - index)
            } else {
                geometry.center_x(index - range.start)
            };
            let point = LogicalPoint::new(x, y_for(*value));
            match previous {
                Some((from, joined)) => {
                    if joined {
                        mesh.push_join(from);
                    }
                    mesh.push_segment(from, point);
                    previous = Some((point, true));
                }
                None => previous = Some((point, false)),
            }
        }
        mesh.build()
    }
}

#[derive(Default)]
struct MeshBuilder {
    vertex_bytes: Vec<u8>,
    index_bytes: Vec<u8>,
    vertex_count: u32,
    min: LogicalPoint,
    max: LogicalPoint,
    any: bool,
}

impl MeshBuilder {
    fn push_vertex(&mut self, point: LogicalPoint) {
        let LogicalPoint { x, y } = point;
        self.vertex_bytes
            .extend_from_slice(&f32::from(x).to_le_bytes());
        self.vertex_bytes
            .extend_from_slice(&f32::from(y).to_le_bytes());
        self.vertex_bytes
            .extend_from_slice(&(-1.0_f32).to_le_bytes());
        if self.any {
            self.min.x = self.min.x.min(x);
            self.min.y = self.min.y.min(y);
            self.max.x = self.max.x.max(x);
            self.max.y = self.max.y.max(y);
        } else {
            self.min = point;
            self.max = point;
            self.any = true;
        }
        self.vertex_count += 1;
    }

    fn push_triangle(&mut self, [a, b, c]: [u32; 3]) {
        for index in [a, b, c] {
            self.index_bytes.extend_from_slice(&index.to_le_bytes());
        }
    }

    fn push_segment(&mut self, from: LogicalPoint, to: LogicalPoint) {
        let dx = f32::from(to.x - from.x);
        let dy = f32::from(to.y - from.y);
        let length = (dx * dx + dy * dy).sqrt();
        if length <= f32::EPSILON {
            return;
        }
        let half = (LINE_WIDTH / 2.0).value();
        let nx = -dy / length * half;
        let ny = dx / length * half;
        let base = self.vertex_count;
        self.push_vertex(LogicalPoint::new(from.x + nx, from.y + ny));
        self.push_vertex(LogicalPoint::new(from.x - nx, from.y - ny));
        self.push_vertex(LogicalPoint::new(to.x + nx, to.y + ny));
        self.push_vertex(LogicalPoint::new(to.x - nx, to.y - ny));
        self.push_triangle([base, base + 1, base + 2]);
        self.push_triangle([base + 2, base + 1, base + 3]);
    }

    fn push_join(&mut self, at: LogicalPoint) {
        let half = LINE_WIDTH / 2.0;
        let base = self.vertex_count;
        self.push_vertex(LogicalPoint::new(at.x - half, at.y - half));
        self.push_vertex(LogicalPoint::new(at.x + half, at.y - half));
        self.push_vertex(LogicalPoint::new(at.x - half, at.y + half));
        self.push_vertex(LogicalPoint::new(at.x + half, at.y + half));
        self.push_triangle([base, base + 1, base + 2]);
        self.push_triangle([base + 2, base + 1, base + 3]);
    }

    fn build(self) -> Option<LineMesh> {
        if !self.any || self.vertex_count == 0 {
            return None;
        }
        Some(LineMesh {
            vertex_bytes: self.vertex_bytes,
            index_bytes: self.index_bytes,
            bounds: LogicalRect::from_origin_size(
                self.min,
                LogicalSize::new(self.max.x - self.min.x, self.max.y - self.min.y),
            ),
        })
    }
}

struct ChartPainter<'a, 'frame> {
    app: &'a mut App,
    text: &'a mut ChartTextCache,
    readout: &'a mut ReadoutCache,
    translate: &'a Translate,
    display: Option<&'a DisplaySnapshot>,
    frame: &'a ChartFrame<'frame>,
    layout: &'a ChartLayout,
    visible: Option<&'a VisibleChart>,
    series: &'a [IndicatorSeries],
    colors: ChartPalette,
    bounds: LogicalRect,
    canvas: Canvas,
    geometry: Vec<GeometryId>,
    price_tick: Tick,
}

impl ChartPainter<'_, '_> {
    fn paint(&mut self) -> Result<(), EngineError> {
        let layout = self.layout;
        let grid = self.colors.chart.grid;
        let data_bounds = layout.data_bounds();
        let _ = self.canvas.push_clip(ClipRect {
            rect: self.bounds,
            corner_radius: [px(0.0); 4],
        });
        self.push_quad(
            LogicalRect::new(
                layout.price_plot.max().x,
                layout.price_plot.origin.y,
                LINE_WIDTH,
                data_bounds.size.height,
            ),
            grid,
        );
        for separator in &layout.separators {
            self.push_quad(*separator, grid);
        }
        self.push_quad(
            LogicalRect::new(
                data_bounds.origin.x,
                data_bounds.max().y,
                data_bounds.size.width,
                LINE_WIDTH,
            ),
            grid,
        );

        if let Some(visible) = self.visible {
            self.paint_visible(visible)?;
        }
        let _ = self.canvas.pop_clip();
        Ok(())
    }

    fn paint_visible(&mut self, visible: &VisibleChart) -> Result<(), EngineError> {
        let layout = self.layout;
        let frame = self.frame;
        let all_series = self.series;
        let translate = self.translate;
        let display = self.display;
        self.paint_price_axis(visible);
        self.paint_time_axis(visible);
        let hovered_slot = frame
            .viewport
            .cursor
            .filter(|cursor| layout.contains_data_point(*cursor))
            .and_then(|cursor| visible.geometry.slot_at(cursor.x));
        for (index, sub_plot) in layout.sub_plots.iter().enumerate() {
            let Some(prepared) = visible.sub_plots.get(index) else {
                continue;
            };
            let series = all_series.iter().find(|series| series.id == prepared.id);
            self.paint_sub_plot(prepared, series, *sub_plot, visible, hovered_slot)?;
        }
        self.paint_main_indicator_lines(visible)?;
        self.paint_candle_series(visible, hovered_slot);
        if let Some(latest) = &visible.latest_price {
            self.paint_latest_price_indicator(latest);
        }
        let absolute = readout_candle_index(frame.viewport.cursor, layout, visible);
        let candles = frame.snapshot.candles();
        let candle = &candles[absolute];
        let previous = absolute.checked_sub(1).and_then(|index| candles.get(index));
        let volume_unit = traded_unit_label(candle, translate, display);
        let spec = ReadoutSpec {
            price_tick: self.price_tick,
            interval: frame.interval,
            zone: frame.data_time_zone,
            volume_unit: &volume_unit,
            muted_color: self.colors.muted_text,
            candle_color: candle_color(candle, &self.colors.chart),
        };
        let readout = self.readout.presentation(self.app, candle, previous, &spec);
        let legend_rows = main_indicator_legend(all_series, absolute, self.price_tick);
        if !legend_rows.is_empty() {
            self.paint_legend_rows(&legend_rows, layout.price_plot, MAIN_LEGEND_TOP);
        }
        let axis_color = self.colors.chart.axis;
        for (index, prepared) in visible.sub_plots.iter().enumerate() {
            let Some(sub_plot) = layout.sub_plots.get(index) else {
                continue;
            };
            let entries = sub_plot_legend(
                prepared.id,
                all_series,
                absolute,
                frame.snapshot,
                self.price_tick,
                axis_color,
            );
            if !entries.is_empty() {
                self.paint_legend(&entries, sub_plot.plot, FIRST_TEXT_ROW_TOP);
            }
        }
        if let Some(cursor) = frame
            .viewport
            .cursor
            .filter(|cursor| layout.contains_data_point(*cursor))
        {
            self.paint_crosshair(cursor, visible, &readout.time_label);
        }
        self.paint_readout(&readout, layout.price_plot);
        Ok(())
    }

    fn label_style(&self, color: Color) -> TextStyle {
        style_of(AXIS_FONT_SIZE, color)
    }

    fn push_quad(&mut self, bounds: LogicalRect, fill: Color) {
        self.canvas.push_quad(Quad {
            bounds,
            corner_radius: [px(0.0); 4],
            paint: QuadPaint::Solid { fill, border: None },
        });
    }

    fn push_text(&mut self, text: &str, style: &TextStyle, origin: LogicalPoint) -> bool {
        if text.is_empty() {
            return true;
        }
        let line = self.text.line(self.app, text, style);
        self.canvas.push_shaped_text(&line, origin)
    }

    fn push_centered_text(
        &mut self,
        text: &str,
        style: &TextStyle,
        center_x: LogicalPx,
        top: LogicalPx,
    ) -> bool {
        if text.is_empty() {
            return true;
        }
        let line = self.text.line(self.app, text, style);
        let origin = LogicalPoint::new(center_x - line.size().width / 2.0, top);
        self.canvas.push_shaped_text(&line, origin)
    }

    fn push_dashed_horizontal(&mut self, y: LogicalPx, plot: LogicalRect, color: Color) {
        for_each_horizontal_dash(y, plot, |bounds| self.push_quad(bounds, color));
    }

    fn push_dashed_vertical(&mut self, x: LogicalPx, plot: LogicalRect, color: Color) {
        for_each_vertical_dash(x, plot, |bounds| self.push_quad(bounds, color));
    }

    fn push_polyline(
        &mut self,
        mesh: LineMesh,
        fill: Color,
        clip: LogicalRect,
    ) -> Result<(), EngineError> {
        let id = self.app.geometry_mut().insert(GeometryDesc {
            vertex_bytes: mesh.vertex_bytes.into(),
            index_bytes: mesh.index_bytes.into(),
            vertex_stride: GeometryPipeline::Ui.vertex_stride(),
            index_type: IndexType::U32,
            topology: Topology::TriangleList,
            bounds: mesh.bounds,
            pipeline: GeometryPipeline::Ui,
        })?;
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [px(0.0); 4],
        });
        self.canvas.push_path(Path {
            bounds: mesh.bounds,
            geometry: id,
            fill,
        });
        let _ = self.canvas.pop_clip();
        self.geometry.push(id);
        Ok(())
    }

    fn paint_main_indicator_lines(&mut self, visible: &VisibleChart) -> Result<(), EngineError> {
        let price_plot = self.layout.price_plot;
        for series in self.series {
            if definition_of(series.id).pane != IndicatorPane::Main {
                continue;
            }
            for line in &series.lines {
                if line.shape != LineShape::Line {
                    continue;
                }
                let Some(mesh) = LineMesh::polyline(
                    &line.values,
                    visible.range.clone(),
                    visible.geometry,
                    |value| y_for_price(value, visible.lowest, visible.highest, price_plot),
                ) else {
                    continue;
                };
                self.push_polyline(mesh, line.color, price_plot)?;
            }
        }
        Ok(())
    }

    fn paint_legend(&mut self, entries: &[LegendEntry], plot: LogicalRect, top: LogicalPx) {
        let mut x = plot.origin.x + TEXT_ROW_INSET;
        let y = plot.origin.y + top;
        let right = plot.max().x;
        let _ = self.canvas.push_clip(ClipRect {
            rect: plot,
            corner_radius: [px(0.0); 4],
        });
        for entry in entries {
            if x >= right {
                break;
            }
            let style = self.label_style(entry.color);
            let line = self.text.line(self.app, &entry.text, &style);
            let width = line.size().width;
            let _ = self.canvas.push_shaped_text(&line, LogicalPoint::new(x, y));
            x += width + LEGEND_ENTRY_GAP;
        }
        let _ = self.canvas.pop_clip();
    }

    fn paint_legend_rows(&mut self, rows: &[Vec<LegendEntry>], plot: LogicalRect, top: LogicalPx) {
        let mut row_top = top;
        for entries in rows {
            if entries.is_empty() {
                continue;
            }
            self.paint_legend(entries, plot, row_top);
            row_top += TEXT_ROW_HEIGHT;
        }
    }

    fn paint_price_axis(&mut self, visible: &VisibleChart) {
        let layout = self.layout;
        let grid = self.colors.chart.grid;
        for tick in &visible.price_ticks {
            let y = y_for_price(
                tick.value,
                visible.lowest,
                visible.highest,
                layout.price_plot,
            );
            self.push_quad(
                LogicalRect::new(
                    layout.price_plot.origin.x,
                    y,
                    layout.price_plot.size.width,
                    LINE_WIDTH,
                ),
                grid,
            );
        }
        let style = self.label_style(self.colors.chart.axis);
        let clip = axis_label_clip(layout.price_axis);
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [px(0.0); 4],
        });
        for tick in &visible.price_ticks {
            let y = y_for_price(
                tick.value,
                visible.lowest,
                visible.highest,
                layout.price_plot,
            );
            let origin = LogicalPoint::new(
                layout.price_axis.origin.x + px(7.0),
                axis_label_y(y, layout.price_axis),
            );
            let _ = self.push_text(&tick.label, &style, origin);
        }
        let _ = self.canvas.pop_clip();
    }

    fn paint_time_axis(&mut self, visible: &VisibleChart) {
        let layout = self.layout;
        let grid = self.colors.chart.grid;
        let data_bounds = layout.data_bounds();
        for tick in &visible.time_ticks {
            self.push_quad(
                LogicalRect::new(
                    tick.x,
                    data_bounds.origin.y,
                    LINE_WIDTH,
                    data_bounds.size.height,
                ),
                grid,
            );
        }
        let style = self.label_style(self.colors.chart.axis);
        let _ = self.canvas.push_clip(ClipRect {
            rect: layout.time_axis,
            corner_radius: [px(0.0); 4],
        });
        for tick in &visible.time_ticks {
            let _ = self.push_centered_text(
                &tick.label,
                &style,
                tick.x,
                layout.time_axis.origin.y + px(6.0),
            );
        }
        let _ = self.canvas.pop_clip();
    }

    fn paint_sub_plot(
        &mut self,
        prepared: &PreparedSubPlot,
        series: Option<&IndicatorSeries>,
        sub_plot: SubPlotLayout,
        visible: &VisibleChart,
        hovered_slot: Option<usize>,
    ) -> Result<(), EngineError> {
        let id = prepared.id;
        let value_range = prepared.value_range;
        let plot = sub_plot.plot;
        let grid = self.colors.chart.grid;
        let axis_color = self.colors.chart.axis;
        let guides = series
            .map(|series| series.guides.as_slice())
            .unwrap_or_default();
        let axis = sub_plot_axis(id, value_range, plot.size.height, guides);
        for value in axis.grid {
            let y = y_for_sub_value(value, value_range, plot);
            self.push_quad(
                LogicalRect::new(plot.origin.x, y, plot.size.width, LINE_WIDTH),
                grid,
            );
        }
        let style = self.label_style(axis_color);
        let clip = axis_label_clip(sub_plot.axis);
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [px(0.0); 4],
        });
        for value in axis.labels {
            let y = y_for_sub_value(value, value_range, plot);
            let label = match id {
                IndicatorId::Vol => format_volume(value),
                IndicatorId::Macd => format_signed_price(value, self.price_tick),
                _ => format_percentage_tick(value),
            };
            let origin = LogicalPoint::new(
                sub_plot.axis.origin.x + px(7.0),
                axis_label_y(y, sub_plot.axis),
            );
            let _ = self.push_text(&label, &style, origin);
        }
        let _ = self.canvas.pop_clip();

        let Some(series) = series else {
            return Ok(());
        };
        for guide in &axis.guides {
            let y = y_for_sub_value(guide.value, value_range, plot);
            if guide.solid {
                self.push_quad(
                    LogicalRect::new(plot.origin.x, y, plot.size.width, LINE_WIDTH),
                    grid,
                );
            } else {
                self.push_dashed_horizontal(y, plot, grid);
            }
        }

        let bars = BarColors::of(self.colors);
        let body_width = visible.geometry.body_width();
        let zero_y = y_for_sub_value(0.0, value_range, plot);
        let _ = self.canvas.push_clip(ClipRect {
            rect: plot,
            corner_radius: [px(0.0); 4],
        });
        for line in &series.lines {
            if line.shape != LineShape::Histogram {
                continue;
            }
            for (slot, absolute) in (visible.range.start..visible.range.end).enumerate() {
                let Some(value) = line.values.get(absolute).copied().flatten() else {
                    continue;
                };
                let value_y = y_for_sub_value(value, value_range, plot);
                let top = value_y.min(zero_y);
                let height = (value_y - zero_y).abs().max(px(1.0));
                let x = visible.geometry.center_x(slot) - body_width / 2.0;
                let color = histogram_bar_color(value, hovered_slot == Some(slot), bars);
                self.push_quad(LogicalRect::new(x, top, body_width, height), color);
            }
        }
        let _ = self.canvas.pop_clip();

        for line in &series.lines {
            if line.shape != LineShape::Line {
                continue;
            }
            let Some(mesh) = LineMesh::polyline(
                &line.values,
                visible.range.clone(),
                visible.geometry,
                |value| y_for_sub_value(value, value_range, plot),
            ) else {
                continue;
            };
            self.push_polyline(mesh, line.color, plot)?;
        }
        Ok(())
    }

    fn paint_candle_series(&mut self, visible: &VisibleChart, hovered: Option<usize>) {
        let layout = self.layout;
        let bullish = self.colors.chart.bullish;
        let bearish = self.colors.chart.bearish;
        let bars = BarColors::of(self.colors);
        let _ = self.canvas.push_clip(ClipRect {
            rect: layout.data_bounds(),
            corner_radius: [px(0.0); 4],
        });
        for (index, candle) in visible.candles.iter().enumerate() {
            let color = if candle.is_up { bullish } else { bearish };
            self.push_quad(candle.wick, color);
            if let Some(bounds) = candle.volume {
                let volume = bars.bar(candle.is_up, hovered == Some(index));
                self.push_quad(bounds, volume);
            }
        }
        let _ = self.canvas.pop_clip();
        let _ = self.canvas.push_clip(ClipRect {
            rect: layout.price_plot,
            corner_radius: [px(0.0); 4],
        });
        for candle in &visible.candles {
            let color = if candle.is_up { bullish } else { bearish };
            self.push_quad(candle.body, color);
        }
        let _ = self.canvas.pop_clip();
    }

    fn paint_latest_price_indicator(&mut self, latest: &PreparedLatestPrice) {
        let y = latest.y;
        let color = if latest.is_up {
            self.colors.chart.bullish
        } else {
            self.colors.chart.bearish
        };
        self.push_dashed_horizontal(y, self.layout.price_plot, color);
        self.paint_latest_price_axis_tag(latest, color);
    }

    fn paint_latest_price_axis_tag(&mut self, latest: &PreparedLatestPrice, background: Color) {
        let axis = self.layout.price_axis;
        let tag_height = LATEST_PRICE_TAG_HEIGHT.min(axis.size.height);
        if tag_height <= LogicalPx::ZERO {
            return;
        }
        let tag_y = (latest.y - LATEST_PRICE_TAG_PRICE_HEIGHT / 2.0)
            .max(axis.origin.y)
            .min(axis.max().y - tag_height);
        self.push_quad(
            LogicalRect::new(axis.origin.x, tag_y, axis.size.width, tag_height),
            background,
        );

        let style = self.label_style(self.colors.tag_foreground);
        let clip = LogicalRect::new(
            axis.origin.x,
            tag_y,
            axis_text_width(axis.size.width, AXIS_TAG_TEXT_INSET),
            tag_height,
        );
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [px(0.0); 4],
        });
        if tag_height < px(34.0) {
            let _ = self.push_text(
                &latest.label,
                &style,
                LogicalPoint::new(
                    axis.origin.x + px(5.0),
                    tag_y + (tag_height - TEXT_ROW_HEIGHT).max(LogicalPx::ZERO) / 2.0,
                ),
            );
            let _ = self.canvas.pop_clip();
            return;
        }
        let _ = self.push_text(
            &latest.label,
            &style,
            LogicalPoint::new(axis.origin.x + px(5.0), tag_y + px(3.0)),
        );
        let _ = self.push_text(
            self.frame.countdown_label,
            &style,
            LogicalPoint::new(
                axis.origin.x + px(5.0),
                tag_y + LATEST_PRICE_TAG_PRICE_HEIGHT,
            ),
        );
        let _ = self.canvas.pop_clip();
    }

    fn paint_crosshair(&mut self, cursor: LogicalPoint, visible: &VisibleChart, time_label: &str) {
        let layout = self.layout;
        let Some(slot) = visible.geometry.slot_at(cursor.x) else {
            return;
        };
        let crosshair = self.colors.chart.crosshair;
        let x = visible.geometry.center_x(slot);
        self.push_dashed_vertical(x, layout.data_bounds(), crosshair);

        if layout.price_plot.contains(cursor) {
            let y = cursor
                .y
                .clamp(layout.price_plot.origin.y, layout.price_plot.max().y);
            self.push_dashed_horizontal(y, layout.price_plot, crosshair);
            let price = price_for_y(y, visible.lowest, visible.highest, layout.price_plot);
            let label = format_price(price, self.price_tick);
            self.paint_axis_tag(y, &label, layout.price_axis);
        } else if let Some((index, sub_plot)) = layout
            .sub_plots
            .iter()
            .enumerate()
            .find(|(_, sub_plot)| sub_plot.plot.contains(cursor))
        {
            let y = cursor
                .y
                .clamp(sub_plot.plot.origin.y, sub_plot.plot.max().y);
            self.push_dashed_horizontal(y, sub_plot.plot, crosshair);
            if let Some(prepared) = visible.sub_plots.get(index) {
                let label = match prepared.id {
                    IndicatorId::Vol => {
                        format_volume(volume_for_y(y, prepared.value_range.highest, sub_plot.plot))
                    }
                    IndicatorId::Macd => format_signed_price(
                        sub_value_from_y(y, prepared.value_range, sub_plot.plot),
                        self.price_tick,
                    ),
                    _ => format_percentage_value(sub_value_from_y(
                        y,
                        prepared.value_range,
                        sub_plot.plot,
                    )),
                };
                self.paint_axis_tag(y, &label, sub_plot.axis);
            }
        }

        let time_axis = layout.time_axis;
        let tag_width = CROSSHAIR_TIME_TAG_WIDTH.min(time_axis.size.width);
        let tag_x = (x - tag_width / 2.0)
            .max(time_axis.origin.x)
            .min(time_axis.max().x - tag_width);
        self.push_quad(
            LogicalRect::new(tag_x, time_axis.origin.y, tag_width, time_axis.size.height),
            self.colors.tag_background,
        );
        let style = self.label_style(self.colors.tag_foreground);
        let clip = LogicalRect::new(tag_x, time_axis.origin.y, tag_width, time_axis.size.height);
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [px(0.0); 4],
        });
        let _ = self.push_centered_text(
            time_label,
            &style,
            tag_x + tag_width / 2.0,
            time_axis.origin.y + px(6.0),
        );
        let _ = self.canvas.pop_clip();
    }

    fn paint_axis_tag(&mut self, y: LogicalPx, label: &str, axis: LogicalRect) {
        let tag_height = CROSSHAIR_TAG_HEIGHT.min(axis.size.height);
        if tag_height <= LogicalPx::ZERO {
            return;
        }
        let tag_y = (y - tag_height / 2.0)
            .max(axis.origin.y)
            .min(axis.max().y - tag_height);
        self.push_quad(
            LogicalRect::new(axis.origin.x, tag_y, axis.size.width, tag_height),
            self.colors.tag_background,
        );
        let style = self.label_style(self.colors.tag_foreground);
        let clip = LogicalRect::new(
            axis.origin.x,
            tag_y,
            axis_text_width(axis.size.width, AXIS_TAG_TEXT_INSET),
            tag_height,
        );
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [px(0.0); 4],
        });
        let _ = self.push_text(
            label,
            &style,
            LogicalPoint::new(axis.origin.x + px(5.0), tag_y + px(3.0)),
        );
        let _ = self.canvas.pop_clip();
    }

    fn paint_readout(&mut self, readout: &ReadoutPresentation, price_plot: LogicalRect) {
        let bounds = readout_bounds(price_plot);
        if bounds.size.is_empty() || readout.width <= 0.0 {
            return;
        }
        let _ = self.canvas.push_clip(ClipRect {
            rect: bounds,
            corner_radius: [px(0.0); 4],
        });
        for run in readout.runs.iter() {
            let _ = self.canvas.push_shaped_text(
                &run.handle,
                LogicalPoint::new(bounds.origin.x + run.x, bounds.origin.y),
            );
        }
        let _ = self.canvas.pop_clip();
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use i18n::language::codes::LanguageCode;
    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        decimal::{Price, Quantity},
        listing::{rules::ListingPrecision, venue::PriceBasis},
        market::{
            book::ListingCandlestick,
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };
    use ui_core::test_support::record::PrimitiveKind;
    use ui_kit::theme::{appearance::Appearance, resolved::Theme};
    use widget::test_support::{CanvasPrimitive, WidgetTestApp};

    use super::*;
    use crate::indicator::IndicatorLine;

    const HARNESS_CANDLES: usize = 200;
    const PAGE_SIZE: (LogicalPx, LogicalPx) = (px(640.0), px(400.0));

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
            color: Color::WHITE,
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

    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: Tick::from_str("0.01").expect("price tick should parse"),
            quantity_tick: Tick::from_str("0.001").expect("quantity step should parse"),
        }
    }

    fn snapshot_of(
        candles: impl IntoIterator<Item = ListingCandlestick>,
    ) -> CandlestickSeriesSnapshot {
        let candles: Vec<ListingCandlestick> = candles.into_iter().collect();
        let mut series = crate::model::CandlestickSeries::new(candles.len().max(1), precision());
        series.merge_initial(candles);
        series.snapshot()
    }

    fn snapshot_with_candles(count: usize) -> CandlestickSeriesSnapshot {
        snapshot_of((0..count).map(|index| candle_with_prices(index as i64 + 1, 100, 110, 90, 100)))
    }

    fn snapshot_with_spacing(count: usize, step_seconds: i64) -> CandlestickSeriesSnapshot {
        const BASE_MILLIS: i64 = 1_700_000_000_000;
        snapshot_of((0..count).map(|index| {
            let open = 100 + index as u64;
            let close = open + 1 + index as u64 % 7;
            candle_with_prices(
                BASE_MILLIS + index as i64 * step_seconds * 1_000,
                open,
                close + 10,
                open - 10,
                close,
            )
        }))
    }

    fn empty_snapshot() -> CandlestickSeriesSnapshot {
        crate::model::CandlestickSeries::new(10, precision()).snapshot()
    }

    fn one_candle_snapshot() -> CandlestickSeriesSnapshot {
        snapshot_of([candle(1)])
    }

    fn test_tick() -> Tick {
        Tick::from_str("0.01").expect("tick should parse")
    }

    fn frame_of<'a>(
        snapshot: &'a CandlestickSeriesSnapshot,
        viewport: ChartViewport,
        indicators: &'a CandlestickIndicatorConfig,
        countdown: &'a SmolStr,
    ) -> ChartFrame<'a> {
        ChartFrame {
            snapshot,
            viewport,
            interval: TimeFrame::Minute1,
            countdown_label: countdown,
            indicators,
            panel_open: false,
            data_time_zone: UtcOffset::UTC,
        }
    }

    fn visible_over(
        snapshot: &CandlestickSeriesSnapshot,
        range: std::ops::Range<usize>,
        layout: &ChartLayout,
    ) -> VisibleChart {
        VisibleChart {
            range,
            lowest: 0.0,
            highest: 100.0,
            geometry: CandleGeometry::new(layout.price_plot, snapshot.candles().len(), px(9.0)),
            candles: Vec::new(),
            price_ticks: Vec::new(),
            time_ticks: Vec::new(),
            latest_price: None,
            sub_plots: Vec::new(),
        }
    }

    fn light_palette() -> ChartPalette {
        ChartPalette::resolve(&Theme::of(Appearance::Light))
    }

    struct Harness {
        harness: WidgetTestApp,
        page: NodeId,
        chart: ChartCanvas,
    }

    impl Harness {
        fn new() -> Self {
            Self::of_size(PAGE_SIZE)
        }

        fn of_size((width, height): (LogicalPx, LogicalPx)) -> Self {
            let mut harness = WidgetTestApp::new();
            let page = harness.with_cx(|cx| {
                let root = cx.tree().root();
                NodeBuilder::boxed()
                    .id("TEST_CANDLESTICK_PAGE")
                    .style(
                        StyleBuilder::new()
                            .width(width)
                            .height(height)
                            .flex_col()
                            .build(),
                    )
                    .build(cx.tree_mut(), root)
                    .expect("the page builds")
            });
            let mut chart = ChartCanvas::new();
            harness
                .with_cx(|cx| chart.build(cx, page))
                .expect("the chart builds its canvas node");
            harness.drain();
            Self {
                harness,
                page,
                chart,
            }
        }

        fn node(&self) -> NodeId {
            self.harness
                .find_by_debug_prefix(self.page, ChartCanvas::selector())
                .expect("the chart built its canvas node")
        }

        fn sync(&mut self, frame: &ChartFrame<'_>) -> bool {
            self.sync_with_display(frame, None)
        }

        fn sync_with_display(
            &mut self,
            frame: &ChartFrame<'_>,
            display: Option<&DisplaySnapshot>,
        ) -> bool {
            self.harness
                .with_cx(|cx| self.chart.sync(cx, frame, display))
                .expect("the chart syncs")
        }

        fn paint(&mut self, frame: &ChartFrame<'_>) {
            assert!(self.sync(frame), "the frame writes a canvas");
            self.harness.drain();
            assert!(self.harness.rendered(), "the frame commits");
        }

        fn primitives(&self) -> Vec<CanvasPrimitive> {
            self.harness.canvas_primitives(self.node())
        }

        fn layout(&self) -> &ChartLayout {
            self.chart.layout().expect("a painted chart has boxes")
        }
    }

    fn kinds(primitives: &[CanvasPrimitive], kind: PrimitiveKind) -> usize {
        primitives.iter().filter(|p| p.kind == kind).count()
    }

    fn candle_bodies(
        primitives: &[CanvasPrimitive],
        colors: ChartPalette,
        body_width: LogicalPx,
    ) -> Vec<LogicalRect> {
        primitives
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .filter(|primitive| {
                primitive.color == Some(colors.chart.bullish)
                    || primitive.color == Some(colors.chart.bearish)
            })
            .filter_map(|primitive| primitive.bounds)
            .filter(|bounds| (bounds.size.width - body_width).abs() < px(0.01))
            .collect()
    }

    fn full_strength_bars_in(
        primitives: &[CanvasPrimitive],
        colors: ChartPalette,
        plot: LogicalRect,
    ) -> Vec<LogicalRect> {
        primitives
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .filter(|primitive| {
                primitive.color == Some(colors.chart.bullish)
                    || primitive.color == Some(colors.chart.bearish)
            })
            .filter_map(|primitive| primitive.bounds)
            .filter(|bounds| bounds.origin.y >= plot.origin.y && bounds.origin.y < plot.max().y)
            .collect()
    }

    fn no_indicators() -> CandlestickIndicatorConfig {
        let mut config = CandlestickIndicatorConfig::default();
        for entry in config.entries.iter_mut() {
            entry.enabled = false;
        }
        config
    }

    fn with_indicators(ids: &[IndicatorId]) -> CandlestickIndicatorConfig {
        let mut config = CandlestickIndicatorConfig::default();
        for entry in config.entries.iter_mut() {
            if ids.contains(&entry.id) {
                entry.enabled = true;
            }
        }
        config
    }

    #[test]
    fn the_canvas_selector_is_the_node_id_the_tests_find() {
        assert_eq!(ChartCanvas::selector(), "CandlestickChart");
    }

    #[test]
    fn a_fresh_canvas_has_no_node_and_no_boxes() {
        let canvas = ChartCanvas::new();

        assert_eq!(canvas.node(), None);
        assert_eq!(canvas.bounds(), None);
        assert!(canvas.layout().is_none());
        assert_eq!(canvas.layout_width(), None);
    }

    #[test]
    fn the_chart_should_write_a_canvas_on_its_first_frame() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);

        harness.paint(&frame);
        let node = harness.node();
        let own = harness.harness.paint_entries(node);
        assert_eq!(
            own.iter()
                .filter(|entry| entry.kind == PrimitiveKind::Canvas)
                .count(),
            1,
            "a self-drawn node is one opaque canvas entry: {own:?}"
        );

        let primitives = harness.primitives();
        assert!(
            kinds(&primitives, PrimitiveKind::Quad) > 0,
            "candles are quads"
        );
        assert!(
            kinds(&primitives, PrimitiveKind::GlyphRun) > 0,
            "axis labels are shaped text"
        );
        assert!(
            kinds(&primitives, PrimitiveKind::Path) > 0,
            "the enabled MA line is a registered path"
        );
        assert_eq!(
            kinds(&primitives, PrimitiveKind::ClipPush),
            kinds(&primitives, PrimitiveKind::ClipPop),
            "every clip bracket is closed"
        );
        assert_eq!(
            primitives.first().map(|primitive| primitive.kind),
            Some(PrimitiveKind::ClipPush),
            "the canvas clips to the node's box first"
        );
        assert_eq!(
            primitives.last().map(|primitive| primitive.kind),
            Some(PrimitiveKind::ClipPop),
            "and closes that clip last"
        );
    }

    #[test]
    fn a_settled_chart_should_not_rewrite_its_canvas() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);

        let node = harness.node();
        let before = harness
            .harness
            .canvas_handle(node)
            .expect("the node holds a canvas");
        let requests = harness.harness.frame_requests();

        assert!(
            !harness.sync(&frame),
            "an unchanged frame reports no change"
        );
        harness.harness.drain();
        assert!(
            !harness.harness.rendered(),
            "an unchanged frame commits nothing"
        );
        assert_eq!(
            harness.harness.frame_requests(),
            requests,
            "a steady frame must not request a frame"
        );
        let after = harness
            .harness
            .canvas_handle(node)
            .expect("the node still holds a canvas");
        assert!(
            Arc::ptr_eq(&before, &after),
            "a steady sync that called set_canvas anyway would replace this value"
        );
    }

    #[test]
    fn a_chart_whose_atlas_lease_died_should_repaint() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);
        let node = harness.node();
        let before = harness
            .harness
            .canvas_handle(node)
            .expect("the node holds a canvas");

        let stack = harness.harness.app().font_stack().clone();
        harness.harness.app_mut().set_font_stack(stack);
        assert!(
            harness.chart.text.has_stale(),
            "the cleared atlas leaves the shaped labels on a dead lease"
        );

        assert!(
            harness.sync(&frame),
            "a dead text lease is a frame that rebuilds"
        );
        harness.harness.drain();
        let after = harness
            .harness
            .canvas_handle(node)
            .expect("the node still holds a canvas");
        assert!(
            !Arc::ptr_eq(&before, &after),
            "the canvas was rebuilt with re-shaped text"
        );
        assert!(
            kinds(&harness.primitives(), PrimitiveKind::GlyphRun) > 0,
            "the labels are re-shaped rather than lost"
        );
        assert!(
            !harness.chart.text.has_stale(),
            "the rebuilt frame re-shaped every line it draws"
        );
    }

    #[test]
    fn changing_the_text_scale_should_reshape_the_text() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &config, &countdown);
        harness.paint(&frame);

        let layout = harness.layout();
        let plot = layout.price_plot;
        let range = visible_range(snapshot.candles(), viewport, plot.size.width);
        let geometry = CandleGeometry::new(plot, range.len(), viewport.candle_width);
        let cursor = LogicalPoint::new(geometry.center_x(10), plot.origin.y + 20.0);
        let hovered = frame_of(
            &snapshot,
            ChartViewport {
                cursor: Some(cursor),
                ..viewport
            },
            &config,
            &countdown,
        );
        assert!(harness.sync(&hovered));
        assert!(
            !harness.chart.text.shaped_scales().is_empty(),
            "the frame shapes its axis labels"
        );
        assert!(
            !harness.chart.readout.shaped_scales().is_empty(),
            "the hovered frame shapes a readout"
        );
        assert!(
            harness
                .chart
                .text
                .shaped_scales()
                .iter()
                .chain(harness.chart.readout.shaped_scales().iter())
                .all(|scale| *scale == 1.0),
            "the harness shapes at the default raster scale"
        );

        harness
            .harness
            .app_mut()
            .set_text_scale(2.0)
            .expect("2.0 is a valid raster scale");
        assert!(
            harness.sync(&hovered),
            "a new raster scale is a different picture"
        );
        harness.harness.drain();

        let labels = harness.chart.text.shaped_scales();
        assert!(
            !labels.is_empty() && labels.iter().all(|scale| *scale == 2.0),
            "every label is re-shaped at the new ppem: {labels:?}"
        );
        let readout = harness.chart.readout.shaped_scales();
        assert!(
            !readout.is_empty() && readout.iter().all(|scale| *scale == 2.0),
            "the readout is re-shaped at the new ppem: {readout:?}"
        );
        assert!(
            kinds(&harness.primitives(), PrimitiveKind::GlyphRun) > 0,
            "the text is re-shaped rather than lost"
        );
    }

    #[test]
    fn switching_the_appearance_should_repaint_the_chart_in_the_new_theme() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);

        let light = ChartPalette::resolve(
            harness
                .harness
                .services()
                .theme
                .theme_for(Appearance::Light),
        );
        let theme = harness.harness.services().theme.clone();
        let dark = ChartPalette::resolve(theme.theme_for(Appearance::Dark));
        assert_ne!(
            light.chart.grid, dark.chart.grid,
            "the two appearances have to differ for this test to mean anything"
        );
        assert!(
            theme.set_appearance(harness.harness.app_mut(), Appearance::Dark),
            "switching the appearance is a write"
        );

        assert!(
            harness.sync(&frame),
            "the resolved colours are part of what the canvas draws"
        );
        harness.harness.drain();
        let primitives = harness.primitives();
        assert!(
            primitives
                .iter()
                .any(|primitive| primitive.color == Some(dark.chart.grid)),
            "the grid is drawn in the dark theme's chart grid"
        );
        assert!(
            !primitives
                .iter()
                .any(|primitive| primitive.color == Some(light.chart.grid)),
            "no light-theme colour survives the switch"
        );
    }

    #[test]
    fn the_axis_should_hold_every_label_the_frame_draws() {
        use crate::chart::geometry::AXIS_LABEL_HORIZONTAL_PADDING;

        let countdown = SmolStr::new_static("00:30");
        let config = CandlestickIndicatorConfig::default();
        let sub_plot_ids = enabled_sub_plot_ids(&config);
        for (size, base_price) in [
            ((px(640.0), px(400.0)), 100_u64),
            ((px(640.0), px(400.0)), 1_234_567_u64),
            ((px(420.0), px(320.0)), 123_456_u64),
        ] {
            let mut harness = Harness::of_size(size);
            let snapshot = snapshot_of((0..HARNESS_CANDLES).map(|index| {
                let open = base_price + index as u64;
                candle_with_prices(index as i64 + 1, open, open + 10, open - 10, open)
            }));
            let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
            harness.paint(&frame);

            let layout = harness.layout();
            let plot = layout.price_plot;
            let axis_width = layout.price_axis.size.width;
            let range = visible_range(
                snapshot.candles(),
                ChartViewport::default(),
                plot.size.width,
            );
            let latest = latest_candle_for_indicator(&snapshot, range.end)
                .map(|candle| CandlePrices::of(candle).close());
            let series = Arc::clone(
                &harness
                    .chart
                    .indicators
                    .as_ref()
                    .expect("the painted frame computed its series")
                    .series,
            );
            let style = style_of(AXIS_FONT_SIZE, Color::WHITE);
            let widest = axis_label_candidates(
                snapshot.candles(),
                range,
                latest,
                snapshot.price_tick(),
                plot.size.height,
                &series,
                &sub_plot_ids,
            )
            .into_iter()
            .map(|label| {
                harness
                    .chart
                    .text
                    .width(harness.harness.app_mut(), &label, &style)
            })
            .fold(LogicalPx::ZERO, LogicalPx::max);

            assert!(
                axis_width + px(0.01) >= widest + AXIS_LABEL_HORIZONTAL_PADDING,
                "the {size:?} chart's axis is {axis_width} wide but the widest label it draws is {widest} wide"
            );
        }
    }

    #[test]
    fn the_readout_should_name_the_traded_size_in_the_published_code() {
        let mut harness = Harness::new();
        let asset = test_support::token(test_support::VENUE, "BTC");
        let snapshot = snapshot_of((0..HARNESS_CANDLES).map(|index| {
            let mut candle = candle_with_prices(index as i64 * 60 + 1, 100, 110, 90, 100);
            candle.traded_quantity = Some(MarketQuantity {
                value: Quantity::from(3_u64),
                unit: QuantityUnit::AssetUnits {
                    asset: asset.clone(),
                },
            });
            candle
        }));
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);

        assert!(harness.sync(&frame));
        let unnamed = harness
            .chart
            .readout
            .described_text()
            .expect("the frame describes a candle");
        assert!(
            !unnamed.contains("BTC"),
            "no catalogue has named the token yet: {unnamed}"
        );

        let entry = test_support::entry(
            &test_support::spot_route(),
            test_support::spot_token(test_support::VENUE, "BTC", "USDT"),
            "BTCUSDT",
        );
        let fixture = test_support::Fixture::new(vec![entry.clone()]);
        test_support::publish_fixture_route(
            fixture.catalog(),
            &test_support::spot_route(),
            vec![entry],
        )
        .expect("the fixture route publishes its listing");
        let display = fixture.catalog().cached_display();
        assert!(
            harness.sync_with_display(&frame, Some(&display)),
            "the display generation is part of what the canvas draws"
        );

        let named = harness
            .chart
            .readout
            .described_text()
            .expect("the frame describes a candle");
        assert!(
            named.contains("BTC"),
            "the readout reads the token's own code: {named}"
        );
    }

    #[test]
    fn releasing_the_chart_should_return_its_geometry() {
        let mut harness = Harness::new();
        let baseline = harness.harness.app().geometry().len();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);

        assert!(
            harness.harness.app().geometry().len() > baseline,
            "the MA polyline is registered geometry"
        );
        let path = harness
            .primitives()
            .into_iter()
            .find(|primitive| primitive.kind == PrimitiveKind::Path)
            .expect("the MA line is a path");
        assert!(path.geometry.is_some(), "a path names a registered mesh");

        let page = harness.page;
        harness.harness.with_cx(|cx| harness.chart.release(cx));
        assert_eq!(
            harness.harness.app().geometry().len(),
            baseline,
            "releasing the chart returns its geometry"
        );
        assert!(
            harness
                .harness
                .find_by_debug_prefix(page, ChartCanvas::selector())
                .is_none(),
            "the chart removes the canvas node it owns"
        );
    }

    #[test]
    fn an_empty_snapshot_should_paint_the_axes_and_no_candles() {
        let mut harness = Harness::new();
        let snapshot = empty_snapshot();
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);

        harness.paint(&frame);
        assert!(harness.chart.layout().is_some(), "the boxes are resolved");
        let primitives = harness.primitives();
        assert!(
            kinds(&primitives, PrimitiveKind::Quad) > 0,
            "the plots' frame is drawn: {primitives:?}"
        );
        assert_eq!(
            kinds(&primitives, PrimitiveKind::GlyphRun),
            0,
            "no candle means no axis label, tag or readout"
        );
        assert_eq!(
            kinds(&primitives, PrimitiveKind::ClipPush),
            kinds(&primitives, PrimitiveKind::ClipPop)
        );
    }

    #[test]
    fn panning_should_move_the_candles_not_the_axis_labels() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let following = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&following);
        let colors = ChartPalette::resolve(
            harness
                .harness
                .services()
                .theme
                .theme_for(Appearance::Light),
        );
        let layout = harness.layout();
        let plot = layout.price_plot;
        let viewport = ChartViewport::default();
        let range = visible_range(snapshot.candles(), viewport, plot.size.width);
        let body_width = CandleGeometry::new(plot, range.len(), viewport.candle_width).body_width();
        let bodies_before = candle_bodies(&harness.primitives(), colors, body_width);
        let grid_before: Vec<LogicalPx> = harness
            .primitives()
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .filter(|primitive| primitive.color == Some(colors.chart.grid))
            .filter_map(|primitive| primitive.bounds)
            .filter(|bounds| bounds.size.width == LINE_WIDTH)
            .map(|bounds| bounds.origin.x)
            .collect();

        let stride = ((px(70.0) / viewport.candle_width).ceil() as usize).max(1);
        assert_eq!(
            HARNESS_CANDLES % stride,
            0,
            "the fixture keeps the tick alignment"
        );
        let pan_candles = stride * 6;
        let anchor = snapshot.candles()[HARNESS_CANDLES - 1 - pan_candles].timestamp;
        let panned = frame_of(
            &snapshot,
            ChartViewport {
                right_timestamp: Some(anchor),
                ..ChartViewport::default()
            },
            &config,
            &countdown,
        );
        assert!(harness.sync(&panned), "a pan is a different picture");
        harness.harness.drain();
        assert!(harness.harness.rendered());

        let bodies_after = candle_bodies(&harness.primitives(), colors, body_width);
        let grid_after: Vec<LogicalPx> = harness
            .primitives()
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .filter(|primitive| primitive.color == Some(colors.chart.grid))
            .filter_map(|primitive| primitive.bounds)
            .filter(|bounds| bounds.size.width == LINE_WIDTH)
            .map(|bounds| bounds.origin.x)
            .collect();
        assert_eq!(
            bodies_before.len(),
            bodies_after.len(),
            "the visible window is the same length"
        );
        assert_eq!(
            grid_before, grid_after,
            "the vertical axis grid sits in the same slots"
        );
        assert_ne!(
            bodies_before, bodies_after,
            "the candles drawn are the window's own"
        );
    }

    #[test]
    fn a_frame_without_a_pointer_should_still_paint_the_readout_and_its_legend() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);

        let readout = harness
            .chart
            .readout
            .described_text()
            .expect("a frame always describes a candle");
        assert!(!readout.is_empty());
        let texts = harness.chart.text.texts();
        assert!(
            texts.iter().any(|text| text.as_str().starts_with("MA5")),
            "the main-chart legend was shaped: {texts:?}"
        );
        let colors = ChartPalette::resolve(
            harness
                .harness
                .services()
                .theme
                .theme_for(Appearance::Light),
        );
        assert!(
            !harness
                .primitives()
                .iter()
                .any(|primitive| primitive.color == Some(colors.chart.crosshair)),
            "a pointer away from the chart draws no crosshair"
        );
    }

    #[test]
    fn the_readout_should_be_clipped_to_its_row() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);

        let bounds = readout_bounds(harness.layout().price_plot);
        let primitives = harness.primitives();
        let push = primitives
            .iter()
            .position(|primitive| {
                primitive.kind == PrimitiveKind::ClipPush && primitive.bounds == Some(bounds)
            })
            .expect("the readout row is clipped to its own box");
        assert!(
            primitives[push + 1..]
                .iter()
                .take_while(|primitive| primitive.kind != PrimitiveKind::ClipPop)
                .any(|primitive| primitive.kind == PrimitiveKind::GlyphRun),
            "the row's text is painted inside that clip"
        );
    }

    #[test]
    fn panning_one_candle_should_not_recompute_the_indicator_values() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);
        let builds = harness.chart.indicator_builds();
        assert_eq!(builds, 1, "the first frame computes the values once");

        let anchor = snapshot.candles()[HARNESS_CANDLES - 50].timestamp;
        let panned = frame_of(
            &snapshot,
            ChartViewport {
                right_timestamp: Some(anchor),
                ..ChartViewport::default()
            },
            &config,
            &countdown,
        );
        assert!(harness.sync(&panned));
        assert_eq!(
            harness.chart.indicator_builds(),
            builds,
            "panning one candle must not recompute the values"
        );
    }

    #[test]
    fn the_hovered_readout_should_name_the_candle_under_the_pointer() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &config, &countdown);
        harness.paint(&frame);

        let layout = harness.layout();
        let plot = layout.price_plot;
        let range = visible_range(snapshot.candles(), viewport, plot.size.width);
        let geometry = CandleGeometry::new(plot, range.len(), viewport.candle_width);
        let slot = 10;
        let absolute = range.start + slot;
        let cursor = LogicalPoint::new(geometry.center_x(slot), plot.origin.y + 20.0);

        let hovered = frame_of(
            &snapshot,
            ChartViewport {
                cursor: Some(cursor),
                ..viewport
            },
            &config,
            &countdown,
        );
        assert!(harness.sync(&hovered));
        let described = harness
            .chart
            .readout
            .described_text()
            .expect("a hovered frame describes a candle");
        let candle = &snapshot.candles()[absolute];
        let open = candle.open.value.format_and_round_tick(test_tick());
        assert!(
            described.contains(open.as_str()),
            "the readout names candle {absolute} (open {open}): {described}"
        );
    }

    #[test]
    fn the_readout_should_describe_the_newest_visible_candle() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &config, &countdown);
        harness.paint(&frame);

        let range = visible_range(
            snapshot.candles(),
            viewport,
            harness.layout().price_plot.size.width,
        );
        assert!(range.end < snapshot.candles().len() || range.end == snapshot.candles().len());
        let described = harness
            .chart
            .readout
            .described_text()
            .expect("a frame describes a candle");
        let newest = &snapshot.candles()[range.end - 1];
        let open = newest.open.value.format_and_round_tick(test_tick());
        assert!(
            described.contains(open.as_str()),
            "the readout describes the newest visible candle (open {open}): {described}"
        );
    }

    #[test]
    fn the_time_tag_should_name_the_candle_under_the_pointer() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &config, &countdown);
        harness.paint(&frame);

        let plot = harness.layout().price_plot;
        let range = visible_range(snapshot.candles(), viewport, plot.size.width);
        let geometry = CandleGeometry::new(plot, range.len(), viewport.candle_width);
        let slot = 6;
        let absolute = range.start + slot;
        let cursor = LogicalPoint::new(geometry.center_x(slot), plot.origin.y + 20.0);

        let hovered = frame_of(
            &snapshot,
            ChartViewport {
                cursor: Some(cursor),
                ..viewport
            },
            &config,
            &countdown,
        );
        assert!(harness.sync(&hovered));
        let label = harness
            .chart
            .readout
            .described_time_label()
            .expect("a hovered frame tags a time");
        assert_eq!(
            label,
            crate::time::format_crosshair_time(
                snapshot.candles()[absolute].timestamp,
                TimeFrame::Minute1,
                UtcOffset::UTC
            ),
            "the tag names the date of candle {absolute}"
        );
    }

    #[test]
    fn only_the_hovered_macd_bar_should_be_drawn_at_full_strength() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = with_indicators(&[IndicatorId::Macd]);
        let countdown = SmolStr::new_static("00:30");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &config, &countdown);
        harness.paint(&frame);

        let plot = harness.layout().price_plot;
        let range = visible_range(snapshot.candles(), viewport, plot.size.width);
        let geometry = CandleGeometry::new(plot, range.len(), viewport.candle_width);
        let slot = 12;
        let cursor = LogicalPoint::new(geometry.center_x(slot), plot.origin.y + 20.0);
        let hovered = frame_of(
            &snapshot,
            ChartViewport {
                cursor: Some(cursor),
                ..viewport
            },
            &config,
            &countdown,
        );
        assert!(harness.sync(&hovered));
        harness.harness.drain();

        let layout = harness.layout();
        assert_eq!(layout.sub_plots.len(), 2, "VOL and MACD");
        let macd_plot = layout.sub_plots[1].plot;
        let colors = ChartPalette::resolve(
            harness
                .harness
                .services()
                .theme
                .theme_for(Appearance::Light),
        );
        let full = full_strength_bars_in(&harness.primitives(), colors, macd_plot);
        assert_eq!(
            full.len(),
            1,
            "only the bar under the pointer is at full strength: {full:?}"
        );
    }

    #[test]
    fn a_pointer_should_light_the_same_slot_in_both_bar_plots() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = with_indicators(&[IndicatorId::Macd]);
        let countdown = SmolStr::new_static("00:30");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &config, &countdown);
        harness.paint(&frame);

        let plot = harness.layout().price_plot;
        let range = visible_range(snapshot.candles(), viewport, plot.size.width);
        let geometry = CandleGeometry::new(plot, range.len(), viewport.candle_width);
        let slot = 15;
        let cursor = LogicalPoint::new(geometry.center_x(slot), plot.origin.y + 20.0);
        let hovered = frame_of(
            &snapshot,
            ChartViewport {
                cursor: Some(cursor),
                ..viewport
            },
            &config,
            &countdown,
        );
        assert!(harness.sync(&hovered));
        harness.harness.drain();

        let layout = harness.layout();
        let volume_plot = layout.sub_plots[0].plot;
        let macd_plot = layout.sub_plots[1].plot;
        let colors = ChartPalette::resolve(
            harness
                .harness
                .services()
                .theme
                .theme_for(Appearance::Light),
        );
        let primitives = harness.primitives();
        assert_eq!(
            full_strength_bars_in(&primitives, colors, volume_plot).len(),
            1,
            "the volume bar under the pointer is lit"
        );
        assert_eq!(
            full_strength_bars_in(&primitives, colors, macd_plot).len(),
            1,
            "the MACD bar under the pointer is lit"
        );
    }

    #[test]
    fn a_pointer_over_an_axis_should_draw_no_crosshair() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &config, &countdown);
        harness.paint(&frame);

        let axis = harness.layout().price_axis;
        let cursor = LogicalPoint::new(
            axis.origin.x + axis.size.width / 2.0,
            axis.origin.y + axis.size.height / 4.0,
        );
        let over_axis = frame_of(
            &snapshot,
            ChartViewport {
                cursor: Some(cursor),
                ..viewport
            },
            &config,
            &countdown,
        );
        assert!(harness.sync(&over_axis));
        harness.harness.drain();

        let colors = ChartPalette::resolve(
            harness
                .harness
                .services()
                .theme
                .theme_for(Appearance::Light),
        );
        assert!(
            !harness
                .primitives()
                .iter()
                .any(|primitive| primitive.color == Some(colors.chart.crosshair)),
            "a pointer over an axis draws no crosshair"
        );
    }

    #[test]
    fn the_layout_should_draw_one_box_per_enabled_sub_plot() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = with_indicators(&[IndicatorId::Macd, IndicatorId::Rsi]);
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);

        let layout = harness.layout();
        assert_eq!(
            layout.sub_plots.len(),
            3,
            "VOL, MACD and RSI each own a box"
        );
        assert_eq!(layout.separators.len(), layout.sub_plots.len());
        let axis_clips: Vec<LogicalRect> = layout
            .sub_plots
            .iter()
            .map(|sub_plot| axis_label_clip(sub_plot.axis))
            .collect();
        let primitives = harness.primitives();
        for clip in axis_clips {
            assert!(
                primitives.iter().any(|primitive| {
                    primitive.kind == PrimitiveKind::ClipPush && primitive.bounds == Some(clip)
                }),
                "the sub plot at {clip:?} has a labelled axis"
            );
        }
    }

    #[test]
    fn a_chart_without_a_legend_should_show_one_row() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = no_indicators();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);

        assert!(
            harness.chart.readout.described_text().is_some(),
            "the readout is drawn on every frame"
        );
        let texts = harness.chart.text.texts();
        assert!(
            !texts
                .iter()
                .any(|text| text.as_str().starts_with("MA") || text.as_str().starts_with("VOL")),
            "no legend row is shaped without an enabled indicator: {texts:?}"
        );
        assert!(
            kinds(&harness.primitives(), PrimitiveKind::GlyphRun) > 0,
            "the axis labels and the readout are still text"
        );
    }

    #[test]
    fn indicators_of_different_kinds_should_take_a_legend_row_each() {
        let mut harness = Harness::new();
        let snapshot = snapshot_with_spacing(HARNESS_CANDLES, 60);
        let config = with_indicators(&[IndicatorId::Ema]);
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);
        harness.paint(&frame);

        let texts = harness.chart.text.texts();
        assert!(
            texts.iter().any(|text| text.as_str().starts_with("MA5")),
            "the MA row is shaped: {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.as_str().starts_with("EMA12")),
            "the EMA row is shaped too, in a row of its own: {texts:?}"
        );
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
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
            LayoutInputs {
                longest_label_width: LogicalPx::ZERO,
                sub_plot_count: 1,
            },
        );
        let volume_plot = layout.sub_plots[0].plot;
        let geometry = CandleGeometry::new(layout.price_plot, candles.len(), px(5.0));
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
                        .all(|other| !bounds.intersects(*other))
                })
        );
        assert!(body_bounds.iter().enumerate().all(|(index, bounds)| {
            body_bounds[index + 1..]
                .iter()
                .all(|other| !bounds.intersects(*other))
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
        let plot = LogicalRect::new(px(10.0), px(20.0), px(20.0), px(20.0));
        let mut horizontal = Vec::with_capacity(3);
        let mut vertical = Vec::with_capacity(3);
        for_each_horizontal_dash(px(25.0), plot, |bounds| horizontal.push(bounds));
        for_each_vertical_dash(px(15.0), plot, |bounds| vertical.push(bounds));

        assert_eq!(
            horizontal,
            [
                LogicalRect::new(px(10.0), px(25.0), px(5.0), px(1.0)),
                LogicalRect::new(px(19.0), px(25.0), px(5.0), px(1.0)),
                LogicalRect::new(px(28.0), px(25.0), px(2.0), px(1.0)),
            ]
        );
        assert_eq!(
            vertical,
            [
                LogicalRect::new(px(15.0), px(20.0), px(1.0), px(5.0)),
                LogicalRect::new(px(15.0), px(29.0), px(1.0), px(5.0)),
                LogicalRect::new(px(15.0), px(38.0), px(1.0), px(2.0)),
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
            let spacing = (pair[1] - pair[0]) / 1_000.0 * f64::from(height.value());
            spacing >= f64::from(PRICE_AXIS_MIN_LABEL_SPACING.value())
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
    fn readout_bounds_should_not_exceed_narrow_price_plot() {
        let plot = LogicalRect::new(px(10.0), px(20.0), px(6.0), px(10.0));
        let readout = readout_bounds(plot);

        assert_eq!(
            (readout.size.width, readout.size.height),
            (px(0.0), px(2.0))
        );
    }

    #[test]
    fn the_readout_should_take_the_first_text_row() {
        let plot = LogicalRect::new(px(0.0), px(120.0), px(320.0), px(240.0));
        let readout = readout_bounds(plot);

        assert_eq!(
            readout.origin.y,
            plot.origin.y + FIRST_TEXT_ROW_TOP,
            "the readout starts at the plot's first text row"
        );
        assert_eq!(
            readout.size.height, TEXT_ROW_HEIGHT,
            "a tall plot shows one whole readout row"
        );
    }

    #[test]
    fn the_readout_should_have_no_room_in_a_plot_shorter_than_one_row() {
        let plot = |height: LogicalPx| LogicalRect::new(px(0.0), px(0.0), px(320.0), height);

        assert_eq!(
            readout_bounds(plot(TEXT_ROW_INSET)).size.height,
            px(0.0),
            "a plot exactly one inset tall has no row to give"
        );
        assert_eq!(
            readout_bounds(plot(px(10.0))).size.height,
            px(2.0),
            "a plot 10px tall gives what is left after the inset, clipped to one row"
        );
        assert_eq!(
            readout_bounds(plot(px(240.0))).size.height,
            TEXT_ROW_HEIGHT,
            "a plot with room gives exactly one whole row"
        );
    }

    #[test]
    fn the_two_text_rows_should_fit_a_price_plot_at_the_reserved_height() {
        let floor = crate::chart::geometry::MIN_PLOT_HEIGHT;
        let plot = LogicalRect::new(px(0.0), px(0.0), px(320.0), floor);

        let readout = readout_bounds(plot);
        let legend_top = plot.origin.y + MAIN_LEGEND_TOP;

        assert_eq!(
            readout.size.height, TEXT_ROW_HEIGHT,
            "the readout is whole at the floor"
        );
        assert!(
            legend_top >= readout.max().y,
            "the legend row starts at or below the readout's bottom at the floor"
        );
        assert!(
            legend_top + TEXT_ROW_HEIGHT <= plot.max().y,
            "both rows fit the price plot the layout reserves: {legend_top:?} + one row vs {:?}",
            plot.max().y
        );
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

    #[test]
    fn zero_volume_should_map_to_plot_bottom() {
        let plot = LogicalRect::new(px(10.0), px(20.0), px(400.0), px(100.0));

        assert_eq!(y_for_volume(0.0, 100.0, plot), plot.max().y);
    }

    #[test]
    fn maximum_volume_should_map_to_plot_top() {
        let plot = LogicalRect::new(px(10.0), px(20.0), px(400.0), px(100.0));

        assert_eq!(y_for_volume(100.0, 100.0, plot), plot.origin.y);
    }

    #[test]
    fn volume_hover_midpoint_should_map_to_half_the_upper_bound() {
        let plot = LogicalRect::new(px(10.0), px(20.0), px(400.0), px(100.0));
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

    #[test]
    fn a_canvas_with_a_stale_text_lease_should_not_match() {
        let snapshot = one_candle_snapshot();
        let indicators = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:00");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &indicators, &countdown);
        let key = PaintedKey {
            bounds: LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
            candle_width: viewport.candle_width,
            right_timestamp: viewport.right_timestamp,
            cursor: viewport.cursor,
            interval: frame.interval,
            zone: frame.data_time_zone,
            panel_open: frame.panel_open,
            colors: light_palette(),
            language: LanguageCode::En,
            text_scale: 1.0,
            catalog_generation: 0,
        };
        let painted = PaintedCanvas {
            key,
            snapshot: snapshot.clone(),
            indicators: indicators.clone(),
            countdown_label: countdown.clone(),
        };

        assert!(
            painted.matches(&frame, key, false),
            "nothing moved: the canvas on screen is reused"
        );
        assert!(
            !painted.matches(&frame, key, true),
            "a dead atlas lease is not a match: the labels have to be shaped and drawn again"
        );
    }

    #[test]
    fn a_painted_key_should_reject_every_changed_drawing_input() {
        let snapshot = one_candle_snapshot();
        let bounds = LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0));
        let viewport = ChartViewport {
            candle_width: px(9.0),
            right_timestamp: None,
            cursor: None,
        };
        let colors = light_palette();
        let other_colors = ChartPalette::resolve(&Theme::of(Appearance::Dark));
        let key = PaintedKey {
            bounds,
            candle_width: viewport.candle_width,
            right_timestamp: viewport.right_timestamp,
            cursor: None,
            interval: TimeFrame::Minute1,
            zone: UtcOffset::UTC,
            panel_open: false,
            colors,
            language: LanguageCode::En,
            text_scale: 1.0,
            catalog_generation: 0,
        };
        let unchanged = PaintedKey { ..key };
        assert_eq!(key, unchanged);
        assert_ne!(
            key,
            PaintedKey {
                candle_width: px(10.0),
                ..key
            },
            "zooming changes the candle boxes"
        );
        assert_ne!(
            key,
            PaintedKey {
                right_timestamp: Some(1),
                ..key
            },
            "panning changes which candles are visible"
        );
        assert_ne!(
            key,
            PaintedKey {
                bounds: LogicalRect::new(px(0.0), px(0.0), px(640.0), px(480.0)),
                ..key
            },
            "resizing changes every box"
        );
        assert_ne!(
            key,
            PaintedKey {
                interval: TimeFrame::Minute5,
                ..key
            },
            "the interval changes both time labels and the label stride"
        );
        assert_ne!(
            key,
            PaintedKey {
                zone: UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset"),
                ..key
            },
            "another display zone changes every time label"
        );
        assert_ne!(
            key,
            PaintedKey {
                colors: other_colors,
                ..key
            },
            "another appearance changes every colour"
        );
        assert_ne!(
            key,
            PaintedKey {
                language: LanguageCode::ZhCn,
                ..key
            },
            "another language changes the words"
        );
        assert_ne!(
            key,
            PaintedKey {
                text_scale: 2.0,
                ..key
            },
            "another raster scale changes the shaped glyphs"
        );
        assert_ne!(
            key,
            PaintedKey {
                panel_open: true,
                ..key
            },
            "opening the panel changes the chart width"
        );
        assert_ne!(
            key,
            PaintedKey {
                cursor: Some(LogicalPoint::new(px(1.0), px(2.0))),
                ..key
            },
            "the pointer decides the crosshair and the readout"
        );
        assert_ne!(
            key,
            PaintedKey {
                catalog_generation: 7,
                ..key
            },
            "a new catalogue generation can rename the readout's unit"
        );

        let config = CandlestickIndicatorConfig::default();
        let label = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, viewport, &config, &label);
        let painted = PaintedCanvas {
            key,
            snapshot: snapshot.clone(),
            indicators: config.clone(),
            countdown_label: label.clone(),
        };
        assert!(painted.matches(&frame, key, false));
        let other_snapshot = one_candle_snapshot();
        let other_frame = frame_of(&other_snapshot, viewport, &config, &label);
        assert!(
            !painted.matches(&other_frame, key, false),
            "a new snapshot changes the candles"
        );
        let mut other_config = config.clone();
        other_config.entries[0].enabled = false;
        let other_config_frame = frame_of(&snapshot, viewport, &other_config, &label);
        assert!(
            !painted.matches(&other_config_frame, key, false),
            "switching a main-chart indicator off changes the lines and the bounds"
        );
        let other_label = SmolStr::new_static("00:29");
        let countdown_frame = frame_of(&snapshot, viewport, &config, &other_label);
        assert!(
            !painted.matches(&countdown_frame, key, false),
            "the latest-price tag draws the countdown text"
        );
        assert!(
            !painted.matches(&frame, key, true),
            "a dead text lease must rebuild the canvas"
        );
    }

    #[test]
    fn an_indicator_line_mesh_should_break_at_a_none_and_take_one_candle_from_the_left() {
        let plot = LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0));
        let geometry = CandleGeometry::new(plot, 2, px(10.0));
        let y_for = |value: f64| px(value as f32);

        assert!(
            LineMesh::polyline(&[Some(1.0), None, Some(3.0)], 0..3, geometry, y_for).is_none(),
            "a gap leaves two isolated points, which are not a line"
        );
        assert!(
            LineMesh::polyline(&[Some(1.0), Some(3.0)], 0..1, geometry, y_for).is_none(),
            "the right end takes no extra candle"
        );

        let mesh = LineMesh::polyline(&[Some(1.0), Some(3.0)], 1..2, geometry, y_for)
            .expect("a two-point run is a line");
        assert!(
            mesh.bounds.origin.x < geometry.center_x(0),
            "the mesh starts at the candle left of the first visible slot, got {:?}",
            mesh.bounds
        );
        assert!(
            mesh.bounds.max().x >= geometry.center_x(0),
            "the mesh reaches the first visible slot, got {:?}",
            mesh.bounds
        );
    }

    #[test]
    #[should_panic(expected = "the range indexes the whole snapshot")]
    fn an_indicator_line_mesh_should_reject_a_relative_slice() {
        let plot = LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0));
        let geometry = CandleGeometry::new(plot, 2, px(10.0));

        let _ = LineMesh::polyline(&[Some(1.0), Some(2.0)], 1..3, geometry, |value| {
            px(value as f32)
        });
    }

    #[test]
    fn an_indicator_line_mesh_should_take_one_candle_width_left_of_the_first_slot() {
        let plot = LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0));
        let geometry = CandleGeometry::new(plot, 2, px(10.0));

        assert_eq!(
            geometry.center_x_left_of_first(1),
            geometry.center_x(0) - 10.0
        );
        assert_eq!(geometry.center_x_left_of_first(1), px(75.0));

        let empty = CandleGeometry::new(plot, 0, px(10.0));
        assert_eq!(empty.center_x_left_of_first(1), empty.center_x(0) - 10.0);

        let mesh = LineMesh::polyline(&[Some(1.0), Some(2.0)], 1..2, geometry, |value| {
            px(value as f32)
        })
        .expect("a two-point run is a line");
        assert!(
            !mesh.vertex_bytes.is_empty(),
            "a segment writes its four vertices"
        );
        assert_eq!(
            mesh.vertex_bytes.len() % 12,
            0,
            "the Ui pipeline's vertex stride is 12 bytes"
        );
        assert!(
            mesh.index_bytes.len() >= 6,
            "one segment is two triangles, got {} bytes",
            mesh.index_bytes.len()
        );
        assert!(
            mesh.index_bytes.len().is_multiple_of(4),
            "U32 indices are four bytes each"
        );
    }

    #[test]
    #[should_panic(expected = "range.start <= range.end")]
    fn an_indicator_line_mesh_should_reject_a_reversed_range() {
        let plot = LogicalRect::new(px(0.0), px(0.0), px(100.0), px(100.0));
        let geometry = CandleGeometry::new(plot, 2, px(10.0));
        let start = 2;
        let end = 1;

        let _ = LineMesh::polyline(&[Some(1.0), Some(2.0)], start..end, geometry, |value| {
            px(value as f32)
        });
    }

    #[test]
    fn price_bounds_with_indicators_should_pad_after_merging_the_values() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];
        let scale = Price::from(1_u64).inner() as f64;
        let series = [IndicatorSeries {
            id: IndicatorId::Ma,
            legend: SmolStr::new_static("MA"),
            lines: vec![IndicatorLine {
                label: SmolStr::new_static("MA5"),
                color: Color::WHITE,
                values: vec![Some(Price::from(200_u64).inner() as f64)],
                readable_from: 0,
                shape: LineShape::Line,
            }],
            guides: Vec::new(),
        }];

        let (lowest, highest) = price_bounds_with_indicators(&candles, 0..1, None, &series);

        assert!((lowest / scale - 85.6).abs() < 1e-9, "lowest was {lowest}");
        assert!(
            (highest / scale - 204.4).abs() < 1e-9,
            "highest was {highest}"
        );
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

    #[test]
    #[should_panic(expected = "the range indexes the whole snapshot")]
    fn price_bounds_with_indicators_should_reject_a_relative_slice() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];

        let _ = price_bounds_with_indicators(&candles, 0..2, None, &[]);
    }

    #[test]
    fn price_bounds_with_indicators_should_ignore_sub_plot_series() {
        let candles = [
            candle_with_prices(1, 100, 110, 90, 100),
            candle_with_prices(2, 100, 104, 96, 100),
        ];
        let plain = price_bounds(&candles, None);
        let sub_series = [IndicatorSeries {
            id: IndicatorId::Macd,
            legend: SmolStr::new_static("MACD(12,26,9)"),
            lines: vec![IndicatorLine {
                label: SmolStr::new_static("MACD"),
                color: Color::WHITE,
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

    #[test]
    fn axis_label_candidates_should_include_main_indicator_values() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];
        let tick = Tick::from_str("0.01").expect("tick should parse");
        let series = [IndicatorSeries {
            id: IndicatorId::Ma,
            legend: SmolStr::new_static("MA"),
            lines: vec![IndicatorLine {
                label: SmolStr::new_static("MA5"),
                color: Color::WHITE,
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

    #[test]
    fn the_readout_should_describe_the_candle_under_the_cursor() {
        let layout = ChartLayout::new(
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
            LayoutInputs {
                longest_label_width: LogicalPx::ZERO,
                sub_plot_count: 1,
            },
        );
        let geometry = CandleGeometry::new(layout.price_plot, 10, px(9.0));
        let visible = VisibleChart {
            range: 10..20,
            lowest: 0.0,
            highest: 100.0,
            geometry,
            candles: Vec::new(),
            price_ticks: Vec::new(),
            time_ticks: Vec::new(),
            latest_price: None,
            sub_plots: Vec::new(),
        };
        let cursor = LogicalPoint::new(geometry.center_x(3), layout.price_plot.origin.y + 5.0);

        assert_eq!(
            readout_candle_index(Some(cursor), &layout, &visible),
            13,
            "slot 3 of a range starting at 10 is candle 13"
        );

        let price = Price::from_str("123.45").expect("price should parse");
        let tick = Tick::from_str("0.01").expect("tick should parse");
        let mut values = vec![None; 20];
        values[13] = Some(price.inner() as f64);
        let mut series = [IndicatorSeries {
            id: IndicatorId::Ma,
            legend: SmolStr::new_static("MA"),
            lines: vec![IndicatorLine {
                label: SmolStr::new_static("MA5"),
                color: Color::WHITE,
                values,
                readable_from: 14,
                shape: LineShape::Line,
            }],
            guides: Vec::new(),
        }];
        let absolute = readout_candle_index(Some(cursor), &layout, &visible);

        assert!(
            main_indicator_legend(&series, absolute, tick).is_empty(),
            "a candle below readable_from has no trustworthy value"
        );
        series[0].lines[0].readable_from = absolute;
        let rows = main_indicator_legend(&series, absolute, tick);
        assert_eq!(rows.len(), 1, "one indicator, one row");
        assert_eq!(rows[0].len(), 1, "that indicator has one readable line");
        assert_eq!(rows[0][0].text, "MA5 123.45");
    }

    #[test]
    fn the_readout_should_describe_the_newest_visible_candle_without_a_cursor() {
        let snapshot = snapshot_with_candles(200);
        let layout = ChartLayout::new(
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
            LayoutInputs {
                longest_label_width: LogicalPx::ZERO,
                sub_plot_count: 1,
            },
        );
        let visible = visible_over(&snapshot, 10..20, &layout);
        assert!(visible.range.end < snapshot.candles().len());

        assert_eq!(
            readout_candle_index(None, &layout, &visible),
            19,
            "a viewport of 10..20 ends at candle 19: the snapshot's last is not read"
        );

        let moved = visible_over(&snapshot, 150..175, &layout);
        assert_eq!(
            readout_candle_index(None, &layout, &moved),
            174,
            "a viewport of 150..175 ends at candle 174"
        );

        let over_the_axis = LogicalPoint::new(
            layout.price_axis.origin.x + layout.price_axis.size.width / 2.0,
            layout.price_axis.origin.y + layout.price_axis.size.height / 2.0,
        );
        assert_eq!(
            readout_candle_index(Some(over_the_axis), &layout, &visible),
            19,
            "a pointer over the axis falls back to the newest visible candle"
        );
    }

    #[test]
    fn the_readout_fallback_should_follow_the_viewport_after_a_pan() {
        let snapshot = snapshot_with_candles(200);
        let layout = ChartLayout::new(
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
            LayoutInputs {
                longest_label_width: LogicalPx::ZERO,
                sub_plot_count: 0,
            },
        );

        assert_eq!(
            readout_candle_index(None, &layout, &visible_over(&snapshot, 4..14, &layout)),
            13,
            "the newest visible candle of a viewport of 4..14 is candle 13"
        );
        assert_eq!(
            readout_candle_index(None, &layout, &visible_over(&snapshot, 60..90, &layout)),
            89,
            "the same rule on a viewport of 60..90 is candle 89, not 13"
        );
    }

    #[test]
    fn a_chart_without_enabled_sub_plots_should_have_no_sub_plot_boxes() {
        let layout = ChartLayout::new(
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 0,
            },
        );

        assert!(layout.sub_plots.is_empty());
        assert!(layout.separators.is_empty());
        assert_eq!(layout.time_axis.origin.y, layout.price_plot.max().y);
        assert_eq!(layout.data_bounds(), layout.price_plot);
    }

    #[test]
    fn no_enabled_sub_plot_should_prepare_no_sub_plot_data_or_volume_bars() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 0,
            },
        );
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);

        let visible =
            prepare_visible_chart(&frame, &layout, &[], &[]).expect("ten candles are visible");

        assert!(visible.sub_plots.is_empty());
        assert!(
            visible.candles.iter().all(|candle| candle.volume.is_none()),
            "no VOL sub plot means no volume bars"
        );
    }

    #[test]
    fn a_pane_too_short_for_sub_plots_should_prepare_none() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(60.0)),
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 4,
            },
        );
        assert!(
            layout.sub_plots.is_empty(),
            "60px leaves no room for a sub plot"
        );
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);

        let visible = prepare_visible_chart(
            &frame,
            &layout,
            &[],
            &[
                IndicatorId::Vol,
                IndicatorId::Macd,
                IndicatorId::Rsi,
                IndicatorId::Kdj,
            ],
        )
        .expect("ten candles are visible");

        assert!(visible.sub_plots.is_empty());
        assert!(
            visible.candles.iter().all(|candle| candle.volume.is_none()),
            "a sub plot that is not drawn owns no volume bars"
        );
    }

    #[test]
    fn a_first_sub_plot_that_is_not_volume_should_not_prepare_volume_bars() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
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
        let indicators = [series];
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);

        let visible = prepare_visible_chart(&frame, &layout, &indicators, &[IndicatorId::Macd])
            .expect("ten candles are visible");

        assert_eq!(visible.sub_plots.len(), 1);
        assert_eq!(visible.sub_plots[0].id, IndicatorId::Macd);
        assert!(visible.sub_plots[0].value_range.lowest < 0.0);
        assert!(
            visible.candles.iter().all(|candle| candle.volume.is_none()),
            "the MACD box must not receive volume bars"
        );
    }

    #[test]
    fn the_default_volume_sub_plot_should_prepare_its_bars() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            LogicalRect::new(px(0.0), px(0.0), px(320.0), px(240.0)),
            LayoutInputs {
                longest_label_width: px(50.0),
                sub_plot_count: 1,
            },
        );
        let config = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:30");
        let frame = frame_of(&snapshot, ChartViewport::default(), &config, &countdown);

        let visible = prepare_visible_chart(&frame, &layout, &[], &[IndicatorId::Vol])
            .expect("ten candles are visible");

        assert_eq!(visible.sub_plots.len(), 1);
        assert_eq!(visible.sub_plots[0].id, IndicatorId::Vol);
        assert_eq!(visible.sub_plots[0].value_range.lowest, 0.0);
        assert!(visible.sub_plots[0].value_range.highest > 0.0);
        assert!(
            visible.candles.iter().all(|candle| candle.volume.is_some()),
            "every fixture candle published a traded size"
        );
    }

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

    #[test]
    fn macd_histogram_bars_should_use_the_direction_they_point() {
        let bars = BarColors {
            up: Color::WHITE,
            down: Color::BLACK,
            up_resting: Color::Srgb {
                r: 0.5,
                g: 0.5,
                b: 0.5,
                alpha: 0.22,
            },
            down_resting: Color::Srgb {
                r: 0.2,
                g: 0.2,
                b: 0.2,
                alpha: 0.22,
            },
        };

        assert_eq!(histogram_bar_color(0.0, true, bars), bars.up);
        assert_eq!(histogram_bar_color(1.0, true, bars), bars.up);
        assert_eq!(histogram_bar_color(-1.0, true, bars), bars.down);
        assert_eq!(
            histogram_bar_color(-f64::MIN_POSITIVE, true, bars),
            bars.down,
            "a tiny negative value is still negative"
        );
        assert_eq!(
            histogram_bar_color(1.0, false, bars),
            bars.up_resting,
            "a bar that is not under the pointer is drawn at the resting wash"
        );
        assert_eq!(
            histogram_bar_color(-1.0, false, bars),
            bars.down_resting,
            "and a falling one at its own resting wash"
        );
        assert_eq!(
            histogram_bar_color(0.0, false, bars),
            bars.up_resting,
            "a flat bar rests on the side zero counts as"
        );
    }

    #[test]
    fn a_bar_should_be_drawn_in_the_market_roles() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let theme = Theme::of(appearance);
            let bars = BarColors::of(ChartPalette::resolve(&theme));

            assert_eq!(bars.up, theme.role(Role::MarketUp));
            assert_eq!(bars.down, theme.role(Role::MarketDown));
            assert_eq!(
                bars.up_resting,
                theme.role(Role::MarketUpWeak),
                "a bar at rest is the theme's weak market wash, the same tone a \
                 volume bar at rest is drawn in"
            );
            assert_eq!(bars.down_resting, theme.role(Role::MarketDownWeak));
            assert_ne!(
                bars.up, bars.up_resting,
                "the highlighted bar has to differ from the resting ones, or hovering \
                 would not show which bar is being read"
            );
            assert_ne!(bars.down, bars.down_resting);
        }
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

        assert!(
            (value_range.lowest - -7.5).abs() < 1.0e-9,
            "{value_range:?}"
        );
        assert!(
            (value_range.highest - 157.5).abs() < 1.0e-9,
            "{value_range:?}"
        );
    }

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

    #[test]
    fn the_volume_legend_should_read_the_visible_candle() {
        let snapshot = snapshot_of([candle_with_volume(1, Quantity::from(1_234_u64))]);
        let entries = sub_plot_legend(
            IndicatorId::Vol,
            &[],
            0,
            &snapshot,
            test_tick(),
            Color::WHITE,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "VOL 1.234K");
    }

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
        let axis_color = Color::WHITE;

        let before = sub_plot_legend(IndicatorId::Macd, &series, 14, &snapshot, tick, axis_color);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].text, "MACD(12,26,9)");

        let after = sub_plot_legend(IndicatorId::Macd, &series, 15, &snapshot, tick, axis_color);
        assert_eq!(after.len(), 2);
        assert_eq!(after[1].text, "DIF 0.52");
    }

    #[test]
    fn percentage_sub_plot_legends_should_not_repeat_the_formula() {
        let snapshot = snapshot_with_candles(1);
        let tick = test_tick();
        let axis_color = Color::WHITE;
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

    #[test]
    fn legend_entries_should_carry_the_line_colour() {
        let tick = test_tick();
        let snapshot = snapshot_with_candles(1);
        let axis_color = Color::WHITE;
        let main_color = Color::Srgb {
            r: 0.2,
            g: 0.5,
            b: 0.4,
            alpha: 1.0,
        };
        let sub_color = Color::Srgb {
            r: 0.6,
            g: 0.4,
            b: 0.5,
            alpha: 1.0,
        };

        let main = [test_series(
            IndicatorId::Ma,
            "MA",
            vec![IndicatorLine {
                label: SmolStr::new_static("MA5"),
                color: main_color,
                values: vec![Some(42.0)],
                readable_from: 0,
                shape: LineShape::Line,
            }],
            Vec::new(),
        )];
        let rows = main_indicator_legend(&main, 0, tick);
        assert_eq!(rows.len(), 1, "one indicator, one row");
        assert_eq!(rows[0].len(), 1, "one line, one entry");
        assert_eq!(
            rows[0][0].color, main_color,
            "the main legend shows the line's own colour"
        );

        let kdj = [test_series(
            IndicatorId::Kdj,
            "KDJ(9,3,3)",
            vec![IndicatorLine {
                label: SmolStr::new_static("K"),
                color: sub_color,
                values: vec![Some(52.14)],
                readable_from: 0,
                shape: LineShape::Line,
            }],
            vec![20.0, 80.0],
        )];
        let entries = sub_plot_legend(IndicatorId::Kdj, &kdj, 0, &snapshot, tick, axis_color);
        assert_eq!(entries.len(), 2, "the formula entry then the K line");
        assert_eq!(
            entries[0].color, axis_color,
            "the formula entry belongs to the axis, not to a line"
        );
        assert_eq!(
            entries[1].color, sub_color,
            "the second entry is the line's, in its own colour"
        );
    }

    #[test]
    fn a_tiny_negative_percentage_should_not_read_as_negative_zero() {
        assert_eq!(format_percentage_value(0.0), "0.0");
        assert_eq!(format_percentage_value(-0.04), "0.0");
        assert_eq!(format_percentage_value(-0.06), "-0.1");
        assert_eq!(format_percentage_value(48.26), "48.3");
        assert_eq!(format_percentage_tick(-0.0), "0");
        assert_eq!(format_percentage_tick(0.0), "0");
    }
}
