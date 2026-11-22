//! The canvas that draws one candle chart.
//!
//! The chart is a [`Canvas`] value the widget hands to
//! [`WidgetCx::set_canvas`] when, and only when, what it would draw moved. The
//! inputs are named in one key ([`PaintedKey`]): the box the node was laid out in,
//! the viewport, the candles' revision, the interval, the countdown label, the
//! indicator configuration, the settings panel, the colours the theme resolved for
//! the appearance showing now, the language, the raster scale the labels were
//! shaped at and the published catalogue generation the readout's unit is read
//! from. A steady frame therefore replaces nothing and asks for nothing (I3), and a
//! canvas built for another appearance or another language is the wrong value
//! rather than a value to keep.
//!
//! **The atlas lease is part of the key too.** A canvas names glyph runs leased
//! from the engine's shared atlas; an atlas generation change or a per-tile
//! eviction leaves those runs pointing at reused texture, and the engine drops them
//! at encode time. The shaped lines the canvas was built from are kept beside it
//! ([`super::text::ChartTextCache::has_stale`]), and a dead lease is a frame that
//! rebuilds — otherwise the labels would stay missing for good.
//!
//! # Drawing is `Canvas` primitives
//!
//! Everything is written through [`Canvas`]: quads for candles, wicks, volume
//! bars, grid lines, the axis tags and the readout background; clipped paths over
//! registered geometry for the indicator polylines; `push_shaped_text` for every
//! label, tag and legend entry. A canvas is a value in absolute window
//! coordinates, so the drawing has no element tree of its own and the frame order
//! *is* the order of the calls below (the engine paints primitives in push order).
//!
//! **Indicator polylines are registered geometry.** The engine's [`Path`] primitive
//! is a fill over a mesh in the geometry store, so a frame that draws an indicator
//! line builds its triangle list ([`polyline_mesh`]), registers it, and names the
//! handle in the path. The handles are released when the canvas is replaced and on
//! [`ChartCanvas::release`], so the store's count returns to its baseline.
//!
//! # The layout's fixed point is resolved once, not per frame
//!
//! The y-axis width has to fit the widest label any axis can produce, and widening
//! the axis takes width away from the plot, which changes which candles are
//! visible, which can change the labels. The element-tree version of this chart
//! called [`ChartLayout::new`] up to three times per frame to reach that fixed
//! point. Here it is resolved once per [`ChartCanvas::sync`]: the candidate labels
//! are measured on the *widest* plot the box allows (the axis at its minimum width),
//! and the final boxes are laid out with the widest measured label. Widening the
//! axis only removes candles from the left, and a shorter range can only produce
//! shorter or equal labels, so one measurement is an upper bound of what the final
//! layout draws — no frame feedback loop, no second pass.
//!
//! # What was dropped from the element version
//!
//! The element version's debug rectangles have no `Canvas` equivalent: a canvas'
//! primitives *are* the test read point now, so those ~80 debug-only quads are
//! gone. Paint-order layers are gone too — a canvas draws in push order, which the
//! calls below already spell out. The window's content masks became
//! [`Canvas::push_clip`] / [`Canvas::pop_clip`] brackets.

use std::sync::Arc;

use base_ui::{
    style_builder::StyleBuilder,
    theme::{components::ChartColors, resolved::Theme, role::Role},
};
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
    math::{Point, Rect, Size},
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        canvas::Canvas,
        color::Color,
        geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
        primitive::{ClipRect, Path, Quad, QuadPaint},
        style::StylePatch,
    },
    text::TextStyle,
};
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

/// The selector the canvas node carries, so the view's tests can find it.
const SELECTOR: &str = "CandlestickChart";

/// Distance a tick's label is inset from the axis's own edge.
const AXIS_TICK_TEXT_INSET: f32 = 10.0;
/// Distance a tag's label is inset from the axis's own edge.
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
const TEXT_ROW_INSET: f32 = 4.0;
/// Top inset of the first text row of any plot.
///
/// On the price plot that row is the OHLCV readout; on a sub plot it is the
/// sub-plot legend, because a sub plot has no readout.
///
/// A plot shorter than [`TEXT_ROW_HEIGHT`] plus this inset has no room for that
/// row: [`readout_bounds`] then returns a zero-height box, so the readout is not
/// painted instead of being drawn over the plot's neighbour.
const FIRST_TEXT_ROW_TOP: f32 = TEXT_ROW_INSET;
/// The gap between the readout row and the main-chart legend row below it.
const READOUT_LEGEND_GAP: f32 = 2.0;
/// Top inset of the main-chart indicator legend, which takes the second row.
///
/// The readout is drawn on every frame, so the legend can never take the first row
/// back: the two would be painted on top of each other.
const MAIN_LEGEND_TOP: f32 = FIRST_TEXT_ROW_TOP + TEXT_ROW_HEIGHT + READOUT_LEGEND_GAP;
const VOLUME_HEADROOM: f64 = 1.08;
/// Below five label rows a sub plot labels three values instead of five.
///
/// Five labels need five [`TEXT_ROW_HEIGHT`] rows; a shorter plot would draw the
/// rows on top of each other.
const SUB_PLOT_COMPACT_HEIGHT: f32 = 5.0 * TEXT_ROW_HEIGHT;
/// How wide one crosshair tag is, and how tall.
const CROSSHAIR_TAG_HEIGHT: f32 = 20.0;
const CROSSHAIR_TIME_TAG_WIDTH: f32 = 132.0;
/// The dash pattern of the crosshair and the reference lines, in pixels.
const DASH_LENGTH: f32 = 5.0;
const DASH_GAP: f32 = 9.0;
/// The width of an indicator polyline, in logical pixels.
const LINE_WIDTH: f32 = 1.0;
/// The distance the gap between two legend entries takes.
const LEGEND_ENTRY_GAP: f32 = 10.0;

/// Everything one frame of the chart draws from.
pub(crate) struct ChartFrame<'a> {
    pub snapshot: &'a CandlestickSeriesSnapshot,
    pub viewport: ChartViewport,
    pub interval: TimeFrame,
    pub countdown_label: &'a SmolStr,
    /// The indicator configuration. It decides both the values and the layout.
    pub indicators: &'a CandlestickIndicatorConfig,
    /// Whether the settings panel is open, which decides the chart's width.
    pub panel_open: bool,
    /// The zone the time axis and the crosshair read their dates in.
    pub data_time_zone: UtcOffset,
}

/// The colours one appearance resolves the chart to.
///
/// A canvas bakes its colours in, so they are part of [`PaintedKey`]: an appearance
/// switch is a different drawing, not the same one re-tinted.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ChartPalette {
    /// The grid, axis, crosshair and candle colours the theme's chart recipe names.
    chart: ChartColors,
    /// A volume bar or histogram bar at rest: the direction's weak wash.
    bullish_weak: Color,
    bearish_weak: Color,
    /// The tag the crosshair and the latest price are labelled on.
    tag_background: Color,
    tag_foreground: Color,
    /// Muted text: the readout's keys and separators.
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

/// The canvas node, the boxes it last laid out, and the canvas it was built from.
pub(crate) struct ChartCanvas {
    node: Option<NodeId>,
    /// The box the node was last laid out in, in window coordinates.
    bounds: Option<Rect>,
    /// The boxes the canvas on screen was drawn with, for hit testing and prefetch.
    layout: Option<ChartLayout>,
    /// What the canvas on screen was built from.
    painted: Option<PaintedCanvas>,
    text: ChartTextCache,
    readout: ReadoutCache,
    /// The computed indicator values, keyed by the snapshot and the configuration.
    ///
    /// Values do not depend on the viewport, so panning and zooming must not
    /// recompute them.
    indicators: Option<IndicatorValues>,
    /// Counts real recomputations, so a test can tell a cache hit from a rebuild.
    #[cfg(test)]
    indicator_builds: usize,
    /// The geometry the canvas on screen names, released when it is replaced.
    geometry: Vec<GeometryId>,
}

/// A canvas value and the inputs it was built from.
struct PaintedCanvas {
    key: PaintedKey,
    /// The candles the canvas drew, compared by pointer: snapshots deliberately have
    /// no `PartialEq` (it would compare every candle), so `shares_chart_data_with`
    /// is the revision test.
    snapshot: CandlestickSeriesSnapshot,
    /// The configuration the values and the layout came from.
    indicators: CandlestickIndicatorConfig,
    /// The countdown text the latest-price tag drew.
    countdown_label: SmolStr,
}

impl PaintedCanvas {
    /// Whether this canvas already draws `frame` with `key`.
    fn matches(&self, frame: &ChartFrame<'_>, key: PaintedKey, text_is_stale: bool) -> bool {
        !text_is_stale
            && self.key == key
            && self.snapshot.shares_chart_data_with(frame.snapshot)
            && self.indicators == *frame.indicators
            && self.countdown_label == *frame.countdown_label
    }
}

/// The inputs one canvas was built from, apart from the data itself.
///
/// Everything a frame reads has to be in here, or a change to it would keep serving
/// the canvas from before the change.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintedKey {
    /// The box the node was laid out in.
    bounds: Rect,
    candle_width: f32,
    right_timestamp: Option<i64>,
    /// The pointer, when it is over the chart. A move within one candle is the same
    /// drawing only if the slot it resolves to did not change — the slot is what the
    /// quads depend on, so the pointer itself is the key.
    cursor: Option<Point>,
    interval: TimeFrame,
    /// The zone the time axis and the crosshair formatted their dates in.
    zone: UtcOffset,
    panel_open: bool,
    colors: ChartPalette,
    language: LanguageCode,
    text_scale: f32,
    /// The published catalogue generation the readout's quantity unit is read from.
    catalog_generation: u64,
}

/// The computed indicator values of one snapshot and configuration.
struct IndicatorValues {
    snapshot: CandlestickSeriesSnapshot,
    config: CandlestickIndicatorConfig,
    series: Arc<[IndicatorSeries]>,
}

impl ChartCanvas {
    /// An empty canvas, with no node and no boxes.
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

    /// The canvas node, once the widget's body built it.
    ///
    /// Test-only: the widget finds the canvas through the engine's node tree
    /// (`find_by_debug_prefix`), and production reaches the node through the field.
    #[cfg(test)]
    pub(crate) fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// The box the node was last laid out in, if it has been laid out yet.
    ///
    /// The view's `dispatch` gets every pointer event of the widget's surface (the
    /// settings panel shares it), so it needs the whole canvas box to tell "over the
    /// chart" from "over the panel"; [`Self::layout`] only answers the plot areas.
    pub(crate) fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    /// The boxes the canvas on screen was drawn with, for hit testing and prefetch.
    pub(crate) fn layout(&self) -> Option<&ChartLayout> {
        self.layout.as_ref()
    }

    /// The plot width of the canvas on screen, for the view's prefetch arithmetic.
    #[cfg(test)]
    pub(crate) fn layout_width(&self) -> Option<f32> {
        self.layout
            .as_ref()
            .map(|layout| layout.price_plot.size.width)
    }

    /// The canvas' debug selector, which is also the node's id.
    #[cfg(test)]
    pub(crate) fn selector() -> &'static str {
        SELECTOR
    }

    /// Builds the canvas node into `parent` and remembers it.
    ///
    /// The node starts empty (`Rect::default()`): it has no box until the first
    /// layout, and [`Self::sync`] is what fills it in. It registers two dependencies
    /// on purpose: the appearance (a canvas bakes its colours in, so an appearance
    /// switch has to schedule the frame that rebuilds it) and the language (the
    /// canvas' accessible name and every word on it are resolved in the language
    /// showing now).
    pub(crate) fn build(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let language = cx.language();
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let label = t_smol!(translate, "widget.candlestick.chart");
        let node = NodeBuilder::canvas(Canvas::new(Rect::default()))
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

    /// Rebuilds the canvas the chart is drawn on, reporting whether it changed.
    pub(crate) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        frame: &ChartFrame<'_>,
    ) -> Result<bool, EngineError> {
        let Some(node) = self.node else {
            return Ok(false);
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(false);
        };
        let bounds = target.layout().bounds;
        if bounds.size.is_empty() {
            // The frame that laid the node out has not run yet; the next one will.
            return Ok(false);
        }
        self.bounds = Some(bounds);

        // Everything the canvas is a function of, read from the host: the colours of
        // the appearance showing now (never baked at build time), the language, the
        // raster scale, and the catalogue generation the readout's unit comes from.
        let appearance = cx.theme().current(cx.app());
        let colors = ChartPalette::resolve(cx.theme().theme_for(appearance));
        let language = *cx.read(&cx.language());
        let text_scale = cx.app().text_scale();
        let translate = cx.translate().clone();
        let display = cx
            .services()
            .market_participants()
            .map(|connector| connector.catalog().cached_display());
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

        let (canvas, geometry) = self.paint(
            cx.app_mut(),
            &translate,
            display.as_ref(),
            frame,
            bounds,
            colors,
        )?;
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
            // The canvas the node keeps is the one on screen: the geometry it names
            // is still the geometry it names, and the one just registered has no
            // reader.
            for id in geometry {
                cx.app_mut().geometry_mut().release(id);
            }
        }
        Ok(changed)
    }

    /// Drops everything derived from the data, so the next [`Self::sync`] repaints.
    ///
    /// Called when the widget starts looking at different data. The node stays: the
    /// canvas on screen still names the geometry it was built with, and that geometry
    /// is released by the paint that replaces it (or by [`Self::release`]).
    pub(crate) fn clear(&mut self) {
        self.layout = None;
        self.painted = None;
        self.indicators = None;
        self.text.clear();
        self.readout.clear();
    }

    /// Releases every geometry this canvas registered and takes its node down.
    ///
    /// The node is the canvas' own: the widget's body is built once, so a canvas that
    /// goes away takes its node with it. A node the widget already removed is not a
    /// failure — the framework tears the body down after this hook.
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

    /// The computed series for `snapshot` and `config`, from the cache when possible.
    ///
    /// The input columns are built **only** on a miss: allocating them on every frame
    /// would be the per-frame cost this cache exists to avoid.
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

    /// How many times the indicator values have been recomputed (test read point).
    #[cfg(test)]
    pub(crate) fn indicator_builds(&self) -> usize {
        self.indicator_builds
    }

    /// Resolves the boxes, then draws one frame into a canvas value.
    fn paint(
        &mut self,
        app: &mut App,
        translate: &Translate,
        display: Option<&DisplaySnapshot>,
        frame: &ChartFrame<'_>,
        bounds: Rect,
        colors: ChartPalette,
    ) -> Result<(Canvas, Vec<GeometryId>), EngineError> {
        // A repaint is also the moment the caches lose the lines whose atlas lease is
        // gone: the text this frame asks for is re-shaped on demand, and a label no
        // frame draws again must not keep the canvas repainting for good.
        self.text.drop_stale();
        self.readout.drop_stale();
        let series = self.series(frame.snapshot, frame.indicators);
        // The sub-plot count and their order come from the configuration, not from
        // the computed series: VOL is a sub plot with no computed values.
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

/// One axis tick the frame draws.
struct PreparedAxisTick {
    value: f64,
    label: SmolStr,
}

/// One time tick the frame draws.
struct PreparedTimeTick {
    x: f32,
    label: SmolStr,
}

/// The latest price and the tag that names it.
struct PreparedLatestPrice {
    y: f32,
    label: SmolStr,
    is_up: bool,
}

/// One candle's three boxes.
struct PreparedCandle {
    wick: Rect,
    body: Rect,
    volume: Option<Rect>,
    is_up: bool,
}

/// One sub plot's identity and visible value range.
///
/// The axis ticks are derived at paint time from the range and the plot height, so
/// they are not prepared here; this is what the painter needs to answer which
/// indicator a sub plot shows and what value a y coordinate means.
struct PreparedSubPlot {
    id: IndicatorId,
    value_range: ValueRange,
}

/// One frame's drawing plan, resolved from the layout and the visible candles.
struct VisibleChart {
    /// The absolute candles in the viewport. Never empty: the plan is `None` when the
    /// snapshot has no candle in range.
    range: std::ops::Range<usize>,
    lowest: f64,
    highest: f64,
    geometry: CandleGeometry,
    candles: Vec<PreparedCandle>,
    price_ticks: Vec<PreparedAxisTick>,
    time_ticks: Vec<PreparedTimeTick>,
    latest_price: Option<PreparedLatestPrice>,
    /// One entry per sub plot, aligned with [`ChartLayout::sub_plots`].
    sub_plots: Vec<PreparedSubPlot>,
}

/// Resolves the layout, widening the y-axis until it fits its own labels.
///
/// One measurement, not a loop: the candidate labels are read from the widest plot
/// the box allows (the axis at its minimum width, so at least as many candles as the
/// final layout sees), and the final boxes are laid out with the widest of them.
/// Widening the axis only drops candles from the left, and a shorter range can only
/// produce shorter or equal labels, so the result always fits.
fn resolve_chart_layout(
    app: &mut App,
    text: &mut ChartTextCache,
    frame: &ChartFrame<'_>,
    bounds: Rect,
    colors: ChartPalette,
    series: &[IndicatorSeries],
    sub_plot_ids: &[IndicatorId],
) -> ChartLayout {
    let provisional = ChartLayout::new(
        bounds,
        LayoutInputs {
            longest_label_width: 0.0,
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
    .fold(0.0_f32, f32::max);
    let countdown_width = latest.map_or(0.0, |_| {
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

/// Every label the axes might draw, including the ones rounding could produce.
///
/// The axis is sized once per frame from this list, so it must be a superset: a
/// label that appears later must not widen the axis, because that would take width
/// away from the plot and change the very candles it was measured from. That is also
/// why the price labels are derived from the same [`price_bounds_with_indicators`]
/// the plot uses: a main-chart line that moved the bound must move the candidate set
/// with it.
///
/// The sub plots contribute their labels too: all y-axes share one width, so a
/// signed MACD tick must be able to widen the price axis as well (design §6.1).
fn axis_label_candidates(
    candles: &[ListingCandlestick],
    range: std::ops::Range<usize>,
    latest_price: Option<Price>,
    price_tick: Tick,
    price_plot_height: f32,
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
                // Rounding immediately below a unit boundary can add a digit before
                // the suffix changes.
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
                // Cover both height modes: the compact one labels 0/50/100, the tall
                // one labels the 20/80 guides plus any bound that left the domain.
                // `kdj_axis_values` is the same chooser the painter uses, so the axis
                // is never sized for a label it will not draw.
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

/// The width an axis leaves for its labels once the tick inset is taken off.
fn axis_text_width(axis_width: f32, inset: f32) -> f32 {
    (axis_width - inset).max(0.0)
}

/// The box an axis' labels are clipped to.
fn axis_label_clip(axis: Rect) -> Rect {
    Rect::new(
        axis.origin.x,
        axis.origin.y,
        axis_text_width(axis.size.width, AXIS_TICK_TEXT_INSET),
        axis.size.height,
    )
}

/// The frame's drawing plan: which candles are visible and where every box goes.
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
    // The volume bars live in the VOL sub plot, and only there. When VOL is not the
    // first enabled sub plot there is no box for them, so no bar is prepared and the
    // painter draws none.
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
    // One label every 70px at most, aligned to a multiple of the stride so that
    // panning does not make the labels crawl.
    let label_stride = ((70.0 / frame.viewport.candle_width).ceil() as usize).max(1);
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

/// The market colours a bar is drawn with, at both of the strengths a bar uses.
///
/// A volume bar and a MACD histogram bar are the same kind of mark: its height is
/// its own value and its colour is the direction it points. Both therefore read the
/// market roles from here, and both show that direction the same way — a bar at
/// rest is the weak wash of its direction, and only the bar under the pointer is
/// drawn at full strength. One value for the two plots is what keeps them from
/// drifting into two different ideas of what "highlighted" means.
#[derive(Clone, Copy)]
struct BarColors {
    /// A bar pointing up, at the strength the bar under the pointer takes.
    up: Color,
    /// A bar pointing down, at the same strength.
    down: Color,
    /// A bar pointing up, as one at rest is washed in.
    up_resting: Color,
    /// A bar pointing down, as one at rest is washed in.
    down_resting: Color,
}

impl BarColors {
    /// The four colours of `colors`.
    fn of(colors: ChartPalette) -> Self {
        Self {
            up: colors.chart.bullish,
            down: colors.chart.bearish,
            up_resting: colors.bullish_weak,
            down_resting: colors.bearish_weak,
        }
    }

    /// One bar: its direction, at the strength `hovered` asks for.
    fn bar(self, up: bool, hovered: bool) -> Color {
        match (up, hovered) {
            (true, true) => self.up,
            (true, false) => self.up_resting,
            (false, true) => self.down,
            (false, false) => self.down_resting,
        }
    }
}

/// The colour of one MACD histogram bar.
///
/// A non-negative bar points up and a negative one points down; zero counts as
/// positive, so a flat histogram does not flicker between the two directions. The
/// strength is the pointer's: a bar at rest is washed out like a volume bar, and only
/// the bar under the pointer is drawn at full strength.
fn histogram_bar_color(value: f64, hovered: bool, bars: BarColors) -> Color {
    bars.bar(value >= 0.0, hovered)
}

/// One row of a chart legend.
struct LegendEntry {
    text: SmolStr,
    color: Color,
}

/// The main-chart legend, one row per indicator, for the candle at `absolute`.
///
/// One row per indicator rather than one row for all of them: MA's three lines and
/// EMA's one are two different formulas, and run together in a single row the values
/// give the eye no border between them — the numbers read as one series' worth. The
/// outer vector is the rows, in catalogue order, and each row holds that indicator's
/// own lines.
///
/// A line contributes an entry only once the candle is past its own `readable_from`
/// threshold: the value is defined earlier for EMA and MACD, but it is not yet
/// trustworthy. An indicator with no readable line contributes no row at all, so the
/// rows close up instead of leaving a blank one.
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

/// The legend entries of one sub plot for the candle at `absolute`.
///
/// The index is absolute, like the values it reads: a visible slot would read another
/// candle's values after a pan. A line contributes only past its own `readable_from`;
/// the formula name is drawn even when no value is defined yet.
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
    // RSI's line labels already begin with `RSI`, so repeating the series name would
    // read `RSI RSI6`. The other sub plots show the formula with its parameters,
    // which a line label does not carry.
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

/// The absolute candle index the readout and the legends read.
///
/// It is the candle under the pointer, or the newest visible one when the pointer is
/// away. The visible slot alone is not an index into an `IndicatorLine`: after a pan,
/// slot `n` belongs to another candle, so the addition of `range.start` is required
/// (design §5.6).
fn readout_candle_index(
    cursor: Option<Point>,
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
/// close enough to be meaningful: an indicator for a candle far off screen would have
/// to be clamped onto the axis and would then point at a price that is not there.
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
/// Every source of a bound has to be folded in **before** this runs, or the padding
/// is computed from a narrower range than the one being drawn.
fn padded_price_bounds(lowest: f64, highest: f64) -> (f64, f64) {
    let range = (highest - lowest).max(1.0);
    (lowest - range * 0.04, highest + range * 0.04)
}

/// The price range a frame draws for the candles in `candles` alone.
///
/// The painted chart always goes through [`price_bounds_with_indicators`], even when
/// no indicator is enabled; this plain form is what the bounds tests pin, so it stays
/// test-only rather than growing a production caller with the same body.
#[cfg(test)]
fn price_bounds(candles: &[ListingCandlestick], latest_price: Option<Price>) -> (f64, f64) {
    let (lowest, highest) = price_extremes(candles, latest_price);
    padded_price_bounds(lowest, highest)
}

/// The main-chart price range, including every enabled main-chart indicator value in
/// `range`.
///
/// `candles` is the **whole snapshot** and `range` indexes it; the series' values are
/// indexed the same way. Mixing a relative slice with an absolute range has no
/// consistency constraint at all (a 200-candle slice with `1300..1500` would pass
/// unnoticed), which is why the signature only accepts absolute indices.
///
/// The indicators have to be included because a moving average's window can cross the
/// left edge of the visible range: the MA500 of the leftmost visible candle averages
/// closes from candles the chart is not drawing, which may be above every visible
/// high. Merging them after padding would leave the line outside the range it was
/// padded from, and the plot clip would cut it off at the edge.
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
fn price_axis_values(lowest: f64, highest: f64, plot_height: f32, price_tick: Tick) -> Vec<f64> {
    let range = highest - lowest;
    if !range.is_finite() || range <= 0.0 {
        return vec![lowest];
    }

    let plot_height = f64::from(plot_height.max(0.0));
    if plot_height < f64::from(PRICE_AXIS_MIN_LABEL_SPACING) {
        return vec![(lowest + highest) / 2.0];
    }

    let target_intervals = (plot_height / f64::from(PRICE_AXIS_TARGET_LABEL_SPACING))
        .round()
        .clamp(1.0, MAX_PRICE_AXIS_TARGET_INTERVALS);
    // Nearest-step selection targets visual density; the ceiling step enforces the
    // hard minimum gap.
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

/// The y range of one computed sub plot over the visible window (design §6.4).
///
/// It is derived every frame from `range` rather than stored on the series: a range
/// computed over every loaded candle would flatten the visible curve when a distant
/// candle moved the extremes.
fn sub_value_range(series: &IndicatorSeries, range: std::ops::Range<usize>) -> ValueRange {
    debug_assert!(range.start <= range.end);
    let Some(first_line) = series.lines.first() else {
        // KDJ with every line switched off still owns a sub plot; there is nothing to
        // measure, so it falls back to the percentage domain.
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
            // An all-zero MACD collapses onto the middle line; a zero span would make
            // the y mapping divide by zero.
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
            // A negative J is kept: clamping the lower bound at zero would draw it
            // outside the box (design D11).
            let padding = (highest - lowest) * 0.05;
            ValueRange {
                lowest: lowest - padding,
                highest: highest + padding,
            }
        }
        // A sub plot only ever shows a computed sub-plot indicator; a percentage is
        // the only sensible default for a value that is not a price or a size.
        IndicatorId::Ma | IndicatorId::Ema | IndicatorId::Vol => ValueRange {
            lowest: 0.0,
            highest: 100.0,
        },
    }
}

/// The values a sub plot labels, chosen for its height.
fn sub_axis_values(id: IndicatorId, value_range: ValueRange, plot_height: f32) -> Vec<f64> {
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
        // The main-chart indicators are never painted in a sub plot; the two bounds
        // keep the function total for any caller that asks anyway.
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

/// Everything a sub plot's y-axis draws, with the labels and the solid grid lines
/// kept apart.
///
/// A guide value is labelled by `labels` and painted by `guides`, but it must not
/// also appear in `grid`: a full-width solid line at the same y would fill the gaps of
/// the dashed reference line and make it read as an ordinary axis line. The plan is
/// derived here rather than inside the paint loop so the split is testable without
/// capturing pixels.
#[derive(Debug, PartialEq)]
struct SubPlotAxis {
    labels: Vec<f64>,
    grid: Vec<f64>,
    guides: Vec<SubPlotGuide>,
}

fn sub_plot_axis(
    id: IndicatorId,
    value_range: ValueRange,
    plot_height: f32,
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
/// The design asks for 3–5 labels. [`sub_value_range`] pads the 0..100 domain by 5%
/// on each side, so a padded span is `1.1x` the data span and `span / 22` is exactly
/// that padding. A bound that sits one padding step outside 0 / 100 is therefore the
/// padding itself rather than a J that left the domain; labelling it would stack a
/// tick on top of the 0 / 100 label.
///
/// If J goes below zero *and* above 100 there are six candidates; the fixed endpoint
/// closer to its neighbour is dropped, because those two labels would sit on top of
/// each other while the 20 / 80 guides must survive.
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
/// A small negative J rounds to `-0.0`, which reads as a negative value that is not
/// there; the rounded form is normalised instead.
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
fn y_for_sub_value(value: f64, value_range: ValueRange, plot: Rect) -> f32 {
    let span = (value_range.highest - value_range.lowest).max(f64::MIN_POSITIVE);
    let ratio = ((value_range.highest - value) / span).clamp(0.0, 1.0);
    plot.origin.y + plot.size.height * ratio as f32
}

/// The sub-plot value a y coordinate stands for; the inverse of [`y_for_sub_value`].
fn sub_value_from_y(y: f32, value_range: ValueRange, plot: Rect) -> f64 {
    let ratio = if plot.size.height > 0.0 {
        f64::from((y - plot.origin.y) / plot.size.height)
    } else {
        0.0
    };
    value_range.highest - (value_range.highest - value_range.lowest) * ratio.clamp(0.0, 1.0)
}

fn candle_wick_bounds(
    prices: CandlePrices,
    center_x: f32,
    lowest: f64,
    highest: f64,
    plot: Rect,
) -> Rect {
    let high_y = y_for_price(prices.high().inner() as f64, lowest, highest, plot);
    let low_y = y_for_price(prices.low().inner() as f64, lowest, highest, plot);
    Rect::new(
        center_x - LINE_WIDTH / 2.0,
        high_y,
        LINE_WIDTH,
        (low_y - high_y).max(LINE_WIDTH),
    )
}

fn candle_body_bounds(
    prices: CandlePrices,
    center_x: f32,
    body_width: f32,
    lowest: f64,
    highest: f64,
    plot: Rect,
) -> Rect {
    let open_y = y_for_price(prices.open().inner() as f64, lowest, highest, plot);
    let close_y = y_for_price(prices.close().inner() as f64, lowest, highest, plot);
    Rect::new(
        center_x - body_width / 2.0,
        open_y.min(close_y),
        body_width,
        (close_y - open_y).abs().max(1.0),
    )
}

/// The bar of one candle's traded size, absent when it published none.
fn candle_volume_bounds(
    candle: &ListingCandlestick,
    center_x: f32,
    body_width: f32,
    upper: f64,
    plot: Option<Rect>,
) -> Option<Rect> {
    let plot = plot?;
    let quantity = candle.traded_quantity.as_ref()?;
    if quantity.value.is_zero() {
        return None;
    }
    let top = y_for_volume(quantity.value.inner() as f64, upper, plot);
    Some(Rect::new(
        center_x - body_width / 2.0,
        top,
        body_width,
        (plot.max().y - top).max(1.0),
    ))
}

/// The text one candle's traded size is read in.
///
/// A candle that published no traded size has no unit to name, so the label is empty
/// and the size itself reads as the missing state.
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

/// The text one quantity unit is labelled with.
///
/// A traded size is labelled with the asset it counts, a contract count with the
/// shared name for contracts, and an outcome count with the shared name for outcome
/// tokens. The asset name is the catalog's own term for it, which is why the label is
/// read here rather than from the venue's field names. An amount is named after the
/// asset it is denominated in; a traded size is never one, so that arm is what keeps
/// the match total rather than a case the chart draws.
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

/// The display code of one asset, as the published generation decided it.
///
/// The catalog is the only place a canonical [`AssetId`] becomes a name, and the code
/// it publishes is the whole name of the asset. An asset the published generation
/// does not name is the localized unknown: an identity digest is not a name this
/// chart may print, and a contract address is not one either.
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

/// The box the OHLCV readout line is painted in.
///
/// Zero-sized when the plot has no room for the row, so the caller paints nothing
/// rather than drawing outside the plot.
/// One horizontal dashed line's boxes across `plot`, left to right.
///
/// The dash pattern is a pure function of the box so a test can pin the pattern
/// without capturing pixels: each dash is [`DASH_LENGTH`] long (the last one is cut
/// by the edge) and the gap is [`DASH_GAP`].
fn for_each_horizontal_dash(y: f32, plot: Rect, mut f: impl FnMut(Rect)) {
    let right = plot.max().x;
    let mut x = plot.origin.x;
    while x < right {
        f(Rect::new(x, y, DASH_LENGTH.min(right - x), LINE_WIDTH));
        x += DASH_GAP;
    }
}

/// One vertical dashed line's boxes across `plot`, top to bottom.
fn for_each_vertical_dash(x: f32, plot: Rect, mut f: impl FnMut(Rect)) {
    let bottom = plot.max().y;
    let mut y = plot.origin.y;
    while y < bottom {
        f(Rect::new(x, y, LINE_WIDTH, DASH_LENGTH.min(bottom - y)));
        y += DASH_GAP;
    }
}

fn readout_bounds(price_plot: Rect) -> Rect {
    let top = FIRST_TEXT_ROW_TOP;
    Rect::new(
        price_plot.origin.x + TEXT_ROW_INSET,
        price_plot.origin.y + top,
        (price_plot.size.width - TEXT_ROW_INSET * 2.0).max(0.0),
        (price_plot.size.height - top - TEXT_ROW_INSET).clamp(0.0, TEXT_ROW_HEIGHT),
    )
}

fn y_for_price(price: f64, lowest: f64, highest: f64, plot: Rect) -> f32 {
    let ratio = ((highest - price) / (highest - lowest).max(1.0)).clamp(0.0, 1.0);
    plot.origin.y + plot.size.height * ratio as f32
}

fn price_for_y(y: f32, lowest: f64, highest: f64, plot: Rect) -> f64 {
    let ratio = f64::from((y - plot.origin.y) / plot.size.height);
    highest - (highest - lowest) * ratio.clamp(0.0, 1.0)
}

fn y_for_volume(volume: f64, upper: f64, plot: Rect) -> f32 {
    let ratio = (volume / upper.max(1.0)).clamp(0.0, 1.0);
    plot.max().y - plot.size.height * ratio as f32
}

fn volume_for_y(y: f32, upper: f64, plot: Rect) -> f64 {
    let ratio = f64::from((plot.max().y - y) / plot.size.height);
    upper * ratio.clamp(0.0, 1.0)
}

fn axis_label_y(y: f32, axis: Rect) -> f32 {
    let label_height = TEXT_ROW_HEIGHT.min(axis.size.height);
    (y - label_height / 2.0)
        .max(axis.origin.y)
        .min(axis.max().y - label_height)
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

/// One polyline's triangles, in window logical pixels, ready to be registered.
struct LineMesh {
    vertex_bytes: Vec<u8>,
    index_bytes: Vec<u8>,
    bounds: Rect,
}

impl LineMesh {
    /// The `TriangleList` triangles of one 1px polyline over the visible range.
    ///
    /// `values` is indexed like the **whole snapshot** and `range` is the visible
    /// absolute range, so the two are one coordinate system. The line takes one candle
    /// from the left of the range as well, so the segment entering the plot starts at
    /// the right height; the right end takes nothing extra, because the future is not
    /// drawn. A `None` value breaks the line there: no segment is fabricated across a
    /// gap.
    fn polyline(
        values: &[Option<f64>],
        range: std::ops::Range<usize>,
        geometry: CandleGeometry,
        y_for: impl Fn(f64) -> f32,
    ) -> Option<Self> {
        debug_assert!(range.start <= range.end);
        debug_assert!(
            range.end <= values.len(),
            "the range indexes the whole snapshot, got {range:?} for {} values",
            values.len()
        );

        let mut mesh = MeshBuilder::default();
        // `previous` is `(point, whether a segment already ends there)`: a join only
        // needs filling where two segments meet.
        let mut previous: Option<(Point, bool)> = None;
        let first = range.start.saturating_sub(1);
        for (index, value) in values.iter().enumerate().take(range.end).skip(first) {
            let Some(value) = value else {
                previous = None;
                continue;
            };
            let x = if index < range.start {
                // The extra candle is outside `visible_len`, so `center_x` has no slot
                // for it.
                geometry.center_x_left_of_first(range.start - index)
            } else {
                geometry.center_x(index - range.start)
            };
            let point = Point::new(x, y_for(*value));
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

/// Accumulates the triangles of one polyline.
#[derive(Default)]
struct MeshBuilder {
    vertex_bytes: Vec<u8>,
    index_bytes: Vec<u8>,
    vertex_count: u32,
    min: Point,
    max: Point,
    any: bool,
}

impl MeshBuilder {
    /// One vertex of the engine's `Ui` pipeline: position + coverage distance.
    ///
    /// The distance is the shape's own interior (`-1` logical pixel inside), which is
    /// what makes the analytic-AA fragment stage draw the quad at full coverage.
    fn push_vertex(&mut self, point: Point) {
        let Point { x, y } = point;
        self.vertex_bytes.extend_from_slice(&x.to_le_bytes());
        self.vertex_bytes.extend_from_slice(&y.to_le_bytes());
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

    /// The quad one 1px segment covers.
    fn push_segment(&mut self, from: Point, to: Point) {
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let length = (dx * dx + dy * dy).sqrt();
        if length <= f32::EPSILON {
            return;
        }
        let half = LINE_WIDTH / 2.0;
        let nx = -dy / length * half;
        let ny = dx / length * half;
        let base = self.vertex_count;
        self.push_vertex(Point::new(from.x + nx, from.y + ny));
        self.push_vertex(Point::new(from.x - nx, from.y - ny));
        self.push_vertex(Point::new(to.x + nx, to.y + ny));
        self.push_vertex(Point::new(to.x - nx, to.y - ny));
        self.push_triangle([base, base + 1, base + 2]);
        self.push_triangle([base + 2, base + 1, base + 3]);
    }

    /// A square centred on a joint, so two segments meeting at an angle leave no
    /// notch between their quads.
    fn push_join(&mut self, at: Point) {
        let half = LINE_WIDTH / 2.0;
        let base = self.vertex_count;
        self.push_vertex(Point::new(at.x - half, at.y - half));
        self.push_vertex(Point::new(at.x + half, at.y - half));
        self.push_vertex(Point::new(at.x - half, at.y + half));
        self.push_vertex(Point::new(at.x + half, at.y + half));
        self.push_triangle([base, base + 1, base + 2]);
        self.push_triangle([base + 2, base + 1, base + 3]);
    }

    /// The mesh, or `None` when no segment was emitted (a line with no defined pair of
    /// neighbouring values has nothing to draw).
    fn build(self) -> Option<LineMesh> {
        if !self.any || self.vertex_count == 0 {
            return None;
        }
        Some(LineMesh {
            vertex_bytes: self.vertex_bytes,
            index_bytes: self.index_bytes,
            bounds: Rect::from_origin_size(
                self.min,
                Size::new(self.max.x - self.min.x, self.max.y - self.min.y),
            ),
        })
    }
}

/// One frame's drawing pass over a [`Canvas`].
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
    /// The box the node was laid out in, which the canvas is clipped to.
    bounds: Rect,
    canvas: Canvas,
    /// The geometry registered for this canvas, released when it is replaced.
    geometry: Vec<GeometryId>,
    /// The snapshot's price tick, which the MACD axis and legend format with.
    price_tick: Tick,
}

impl ChartPainter<'_, '_> {
    /// Draws the whole frame into [`Self::canvas`].
    fn paint(&mut self) -> Result<(), EngineError> {
        let layout = self.layout;
        let grid = self.colors.chart.grid;
        let data_bounds = layout.data_bounds();
        // A canvas paints in absolute window coordinates: clipping to the node's own
        // box keeps a tag at the edge from painting over a sibling.
        let _ = self.canvas.push_clip(ClipRect {
            rect: self.bounds,
            corner_radius: [0.0; 4],
        });
        // The divider between the plots and their y-axes.
        self.push_quad(
            Rect::new(
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
        // There is no separator box between the last sub plot and the time axis; this
        // top edge is where that boundary is drawn.
        self.push_quad(
            Rect::new(
                data_bounds.origin.x,
                data_bounds.max().y,
                data_bounds.size.width,
                LINE_WIDTH,
            ),
            grid,
        );

        // A snapshot with no candle in range still gets its axes' frame (the quads
        // above); there are no ticks, tags or data to draw without a candle.
        if let Some(visible) = self.visible {
            self.paint_visible(visible)?;
        }
        let _ = self.canvas.pop_clip();
        Ok(())
    }

    /// Draws everything that needs a candle in range.
    fn paint_visible(&mut self, visible: &VisibleChart) -> Result<(), EngineError> {
        let layout = self.layout;
        // The borrowed halves of the painter are copied out of `self` first: the calls
        // below interleave them with `&mut self` drawing methods.
        let frame = self.frame;
        let all_series = self.series;
        let translate = self.translate;
        let display = self.display;
        self.paint_price_axis(visible);
        // The vertical grid is painted before every plot's data, so a line or a bar is
        // drawn on top of it rather than under it.
        self.paint_time_axis(visible);
        // The visible slot the pointer is on, if it is over any plot. Every bar this
        // frame draws reads it: the candle's volume bar and the MACD histogram's bar
        // both stand at full strength only here.
        let hovered_slot = frame
            .viewport
            .cursor
            .filter(|cursor| layout.contains_data_point(*cursor))
            .and_then(|cursor| visible.geometry.slot_at(cursor.x));
        // A sub plot draws its own axis, guides and data. The boxes and the prepared
        // ranges are aligned by index, and both follow catalogue order.
        for (index, sub_plot) in layout.sub_plots.iter().enumerate() {
            let Some(prepared) = visible.sub_plots.get(index) else {
                continue;
            };
            let series = all_series.iter().find(|series| series.id == prepared.id);
            self.paint_sub_plot(prepared, series, *sub_plot, visible, hovered_slot)?;
        }
        // Main-chart indicator lines sit above the grid and below the candles: a line
        // drawn after the wicks would cover them (design §8.1).
        self.paint_main_indicator_lines(visible)?;
        self.paint_candle_series(visible, hovered_slot);
        if let Some(latest) = &visible.latest_price {
            self.paint_latest_price_indicator(latest);
        }
        // The text rows read the candle the chart is describing: the one under the
        // pointer, or the newest visible one while the pointer is away. The absolute
        // index is what indexes the values; a visible slot alone would read another
        // candle's values after a pan.
        let absolute = readout_candle_index(frame.viewport.cursor, layout, visible);
        let candles = frame.snapshot.candles();
        let candle = &candles[absolute];
        let previous = absolute.checked_sub(1).and_then(|index| candles.get(index));
        // The label the size is read in is the described candle's own unit, so a candle
        // that published none carries the missing state instead of a unit.
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
            // Below the readout, which is drawn on every frame and therefore never
            // gives the first row back.
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
                // A sub plot has no readout, so its legend keeps the first row.
                self.paint_legend(&entries, sub_plot.plot, FIRST_TEXT_ROW_TOP);
            }
        }
        // Only the crosshair's lines and tags follow the pointer; the readout above them
        // does not, and is drawn last so the text stays on top of the dashes.
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

    /// The style every axis label, tag and legend entry uses.
    fn label_style(&self, color: Color) -> TextStyle {
        style_of(AXIS_FONT_SIZE, color)
    }

    /// Pushes one quad.
    fn push_quad(&mut self, bounds: Rect, fill: Color) {
        self.canvas.push_quad(Quad {
            bounds,
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid { fill, border: None },
        });
    }

    /// Shapes `text` and pushes it with its top-left at `origin`.
    ///
    /// A handle whose lease died between shaping and pushing draws nothing this frame;
    /// the next frame re-shapes it (`ChartTextCache::line` drops stale handles).
    fn push_text(&mut self, text: &str, style: &TextStyle, origin: Point) -> bool {
        if text.is_empty() {
            return true;
        }
        let line = self.text.line(self.app, text, style);
        self.canvas.push_shaped_text(&line, origin)
    }

    /// Shapes `text` and pushes it horizontally centred on `center_x`.
    fn push_centered_text(
        &mut self,
        text: &str,
        style: &TextStyle,
        center_x: f32,
        top: f32,
    ) -> bool {
        if text.is_empty() {
            return true;
        }
        let line = self.text.line(self.app, text, style);
        let origin = Point::new(center_x - line.size().width / 2.0, top);
        self.canvas.push_shaped_text(&line, origin)
    }

    /// Pushes one horizontal dashed line across `plot`.
    fn push_dashed_horizontal(&mut self, y: f32, plot: Rect, color: Color) {
        for_each_horizontal_dash(y, plot, |bounds| self.push_quad(bounds, color));
    }

    /// Pushes one vertical dashed line across `plot`.
    fn push_dashed_vertical(&mut self, x: f32, plot: Rect, color: Color) {
        for_each_vertical_dash(x, plot, |bounds| self.push_quad(bounds, color));
    }

    /// Registers one polyline's triangles and draws them as a clipped path.
    fn push_polyline(
        &mut self,
        mesh: LineMesh,
        fill: Color,
        clip: Rect,
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
            corner_radius: [0.0; 4],
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

    /// Draws every enabled main-chart line.
    ///
    /// The line is clipped to the price plot: a value outside the price bounds can
    /// still be produced by axis rounding, and the line must not run into the axis.
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

    /// Draws one legend row in a plot's top-left corner, each entry in its own colour.
    ///
    /// `top` is the row's own inset from the plot's top edge, which is what keeps the
    /// main-chart legend below the readout while a sub plot's legend — whose plot has
    /// no readout — keeps the first row.
    fn paint_legend(&mut self, entries: &[LegendEntry], plot: Rect, top: f32) {
        let mut x = plot.origin.x + TEXT_ROW_INSET;
        let y = plot.origin.y + top;
        let right = plot.max().x;
        let _ = self.canvas.push_clip(ClipRect {
            rect: plot,
            corner_radius: [0.0; 4],
        });
        for entry in entries {
            if x >= right {
                break;
            }
            let style = self.label_style(entry.color);
            let line = self.text.line(self.app, &entry.text, &style);
            let width = line.size().width;
            let _ = self.canvas.push_shaped_text(&line, Point::new(x, y));
            x += width + LEGEND_ENTRY_GAP;
        }
        let _ = self.canvas.pop_clip();
    }

    /// Draws several legend rows, one text row apart, each starting at the plot's left
    /// edge.
    ///
    /// This is the main-chart legend, which has one row per indicator. The rows line up
    /// on that left edge rather than continuing where the row above ended: a row that
    /// began under the previous row's last entry would shift every time a value grew a
    /// digit. An empty row is skipped without taking a row of its own, so the rows
    /// close up rather than leaving a gap where an indicator had nothing to say.
    fn paint_legend_rows(&mut self, rows: &[Vec<LegendEntry>], plot: Rect, top: f32) {
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
                Rect::new(
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
            corner_radius: [0.0; 4],
        });
        for tick in &visible.price_ticks {
            let y = y_for_price(
                tick.value,
                visible.lowest,
                visible.highest,
                layout.price_plot,
            );
            let origin = Point::new(
                layout.price_axis.origin.x + 7.0,
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
                Rect::new(
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
            corner_radius: [0.0; 4],
        });
        for tick in &visible.time_ticks {
            let _ = self.push_centered_text(
                &tick.label,
                &style,
                tick.x,
                layout.time_axis.origin.y + 6.0,
            );
        }
        let _ = self.canvas.pop_clip();
    }

    /// Draws one sub plot: its axis, its guides and its data.
    ///
    /// `series` is `None` for VOL, whose bars are painted with the candles rather than
    /// here; the axis is still drawn for every sub plot.
    ///
    /// `hovered_slot` is the visible slot the pointer is on, in the same numbering as
    /// `range`: a histogram bar there is drawn at full strength while the rest of them
    /// keep the weaker tone, exactly as the volume bars do.
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
        // The axis plan keeps the labels and the solid grid lines apart: a guide is
        // labelled and drawn by its own pass, but it must not also get a grid line,
        // which would fill the gaps of the dashed reference line.
        let guides = series
            .map(|series| series.guides.as_slice())
            .unwrap_or_default();
        let axis = sub_plot_axis(id, value_range, plot.size.height, guides);
        for value in axis.grid {
            let y = y_for_sub_value(value, value_range, plot);
            self.push_quad(
                Rect::new(plot.origin.x, y, plot.size.width, LINE_WIDTH),
                grid,
            );
        }
        let style = self.label_style(axis_color);
        let clip = axis_label_clip(sub_plot.axis);
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [0.0; 4],
        });
        for value in axis.labels {
            let y = y_for_sub_value(value, value_range, plot);
            let label = match id {
                IndicatorId::Vol => format_volume(value),
                IndicatorId::Macd => format_signed_price(value, self.price_tick),
                _ => format_percentage_tick(value),
            };
            let origin = Point::new(sub_plot.axis.origin.x + 7.0, axis_label_y(y, sub_plot.axis));
            let _ = self.push_text(&label, &style, origin);
        }
        let _ = self.canvas.pop_clip();

        let Some(series) = series else {
            return Ok(());
        };
        for guide in &axis.guides {
            let y = y_for_sub_value(guide.value, value_range, plot);
            if guide.solid {
                // MACD's zero line is a solid axis rather than a reference band.
                self.push_quad(
                    Rect::new(plot.origin.x, y, plot.size.width, LINE_WIDTH),
                    grid,
                );
            } else {
                self.push_dashed_horizontal(y, plot, grid);
            }
        }

        let bars = BarColors::of(self.colors);
        let body_width = visible.geometry.body_width();
        let zero_y = y_for_sub_value(0.0, value_range, plot);
        // Histogram bars are painted before the lines so a moving average crossing a bar
        // stays on top of it.
        let _ = self.canvas.push_clip(ClipRect {
            rect: plot,
            corner_radius: [0.0; 4],
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
                let height = (value_y - zero_y).abs().max(1.0);
                let x = visible.geometry.center_x(slot) - body_width / 2.0;
                let color = histogram_bar_color(value, hovered_slot == Some(slot), bars);
                self.push_quad(Rect::new(x, top, body_width, height), color);
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

    /// Paints the visible candles' wicks, volume bars and bodies.
    ///
    /// `hovered` is the visible slot under the pointer. Its volume bar takes the
    /// candle's own colour at full strength, so the bar the user is reading stands out
    /// while the rest of the volume plot keeps the weaker tone. Wicks and volume bars
    /// are drawn before the bodies, so a body covers its own wick.
    fn paint_candle_series(&mut self, visible: &VisibleChart, hovered: Option<usize>) {
        let layout = self.layout;
        let bullish = self.colors.chart.bullish;
        let bearish = self.colors.chart.bearish;
        let bars = BarColors::of(self.colors);
        let _ = self.canvas.push_clip(ClipRect {
            rect: layout.data_bounds(),
            corner_radius: [0.0; 4],
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
            corner_radius: [0.0; 4],
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
        if tag_height <= 0.0 {
            return;
        }
        let tag_y = (latest.y - LATEST_PRICE_TAG_PRICE_HEIGHT / 2.0)
            .max(axis.origin.y)
            .min(axis.max().y - tag_height);
        self.push_quad(
            Rect::new(axis.origin.x, tag_y, axis.size.width, tag_height),
            background,
        );

        let style = self.label_style(self.colors.tag_foreground);
        let clip = Rect::new(
            axis.origin.x,
            tag_y,
            axis_text_width(axis.size.width, AXIS_TAG_TEXT_INSET),
            tag_height,
        );
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [0.0; 4],
        });
        if tag_height < 34.0 {
            let _ = self.push_text(
                &latest.label,
                &style,
                Point::new(
                    axis.origin.x + 5.0,
                    tag_y + (tag_height - TEXT_ROW_HEIGHT).max(0.0) / 2.0,
                ),
            );
            let _ = self.canvas.pop_clip();
            return;
        }
        let _ = self.push_text(
            &latest.label,
            &style,
            Point::new(axis.origin.x + 5.0, tag_y + 3.0),
        );
        let _ = self.push_text(
            self.frame.countdown_label,
            &style,
            Point::new(axis.origin.x + 5.0, tag_y + LATEST_PRICE_TAG_PRICE_HEIGHT),
        );
        let _ = self.canvas.pop_clip();
    }

    /// Draws the crosshair's dashed lines, its axis tag and its bottom time tag.
    ///
    /// Only the parts that follow the pointer are drawn here, and only while the pointer
    /// is over the chart. The OHLCV readout is not one of them: it is drawn on every
    /// frame, after this, so the text stays on top of the dashes.
    fn paint_crosshair(&mut self, cursor: Point, visible: &VisibleChart, time_label: &str) {
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
            Rect::new(tag_x, time_axis.origin.y, tag_width, time_axis.size.height),
            self.colors.tag_background,
        );
        let style = self.label_style(self.colors.tag_foreground);
        let clip = Rect::new(tag_x, time_axis.origin.y, tag_width, time_axis.size.height);
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [0.0; 4],
        });
        let _ = self.push_centered_text(
            time_label,
            &style,
            tag_x + tag_width / 2.0,
            time_axis.origin.y + 6.0,
        );
        let _ = self.canvas.pop_clip();
    }

    fn paint_axis_tag(&mut self, y: f32, label: &str, axis: Rect) {
        let tag_height = CROSSHAIR_TAG_HEIGHT.min(axis.size.height);
        if tag_height <= 0.0 {
            return;
        }
        let tag_y = (y - tag_height / 2.0)
            .max(axis.origin.y)
            .min(axis.max().y - tag_height);
        self.push_quad(
            Rect::new(axis.origin.x, tag_y, axis.size.width, tag_height),
            self.colors.tag_background,
        );
        let style = self.label_style(self.colors.tag_foreground);
        let clip = Rect::new(
            axis.origin.x,
            tag_y,
            axis_text_width(axis.size.width, AXIS_TAG_TEXT_INSET),
            tag_height,
        );
        let _ = self.canvas.push_clip(ClipRect {
            rect: clip,
            corner_radius: [0.0; 4],
        });
        let _ = self.push_text(label, &style, Point::new(axis.origin.x + 5.0, tag_y + 3.0));
        let _ = self.canvas.pop_clip();
    }

    /// Draws the OHLCV readout on the price plot's first text row.
    ///
    /// It is drawn on every frame, so it is not part of the crosshair: the crosshair's
    /// lines and tags appear only while the pointer is over the chart, while this line
    /// always describes a candle — the one under the pointer, or the newest visible one.
    fn paint_readout(&mut self, readout: &ReadoutPresentation, price_plot: Rect) {
        let bounds = readout_bounds(price_plot);
        if bounds.size.is_empty() || readout.width <= 0.0 {
            return;
        }
        let _ = self.canvas.push_clip(ClipRect {
            rect: bounds,
            corner_radius: [0.0; 4],
        });
        for run in readout.runs.iter() {
            let _ = self.canvas.push_shaped_text(
                &run.handle,
                Point::new(bounds.origin.x + run.x, bounds.origin.y),
            );
        }
        let _ = self.canvas.pop_clip();
    }
}

/// The chart's own tests: the box arithmetic, the axis and legend decisions, and the
/// canvas one frame writes.
///
/// Three read points between them cover a frame: the pure helpers (no engine at all),
/// the shaped text the caches hold ([`ChartTextCache::texts`] and
/// [`ReadoutCache::described_text`]), and the canvas primitives the frame painted
/// ([`WidgetTestApp::canvas_primitives`]). The element version recorded its paint in a
/// thread-local log because a window exposed no canvas; the engine's canvas is readable
/// primitive by primitive, so that log is gone (P3-1j).
#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use base_ui::theme::{appearance::Appearance, resolved::Theme};
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
    use widget::test_support::{CanvasPrimitive, WidgetTestApp};

    use super::*;
    use crate::indicator::IndicatorLine;

    /// How many candles a harness snapshot holds.
    ///
    /// Wide enough that the visible window is the same length before and after a pan,
    /// which is what makes "the candles moved, the slots did not" assertable.
    const HARNESS_CANDLES: usize = 200;
    /// The box the harness page gives the chart.
    const PAGE_SIZE: (f32, f32) = (640.0, 400.0);

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

    /// The same, with candles `step_seconds` apart, on a realistic epoch base.
    ///
    /// Timestamps are milliseconds, which is what a candle carries and what the time
    /// formatter reads. The open price moves with the index, so two candles do not
    /// print the same OHLCV line: a test that asserts *which* candle was described
    /// needs the text to tell them apart.
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

    /// One frame's inputs over `snapshot`.
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

    /// A visible plan over an explicit range, for the readout tests.
    fn visible_over(
        snapshot: &CandlestickSeriesSnapshot,
        range: std::ops::Range<usize>,
        layout: &ChartLayout,
    ) -> VisibleChart {
        VisibleChart {
            range,
            lowest: 0.0,
            highest: 100.0,
            geometry: CandleGeometry::new(layout.price_plot, snapshot.candles().len(), 9.0),
            candles: Vec::new(),
            price_ticks: Vec::new(),
            time_ticks: Vec::new(),
            latest_price: None,
            sub_plots: Vec::new(),
        }
    }

    /// The colours the light appearance resolves the chart to.
    fn light_palette() -> ChartPalette {
        ChartPalette::resolve(&Theme::of(Appearance::Light))
    }

    // ── the canvas: the widget's harness over a real chart canvas ────────────────

    /// One chart canvas built into a page, and the page it lives on.
    struct Harness {
        harness: WidgetTestApp,
        page: NodeId,
        chart: ChartCanvas,
    }

    impl Harness {
        fn new() -> Self {
            Self::of_size(PAGE_SIZE)
        }

        fn of_size((width, height): (f32, f32)) -> Self {
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
            // The engine lays the page out in the frame pipeline: the first sync needs
            // a box, so one frame has to run first.
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

        /// Syncs one frame and reports whether a new canvas was written.
        fn sync(&mut self, frame: &ChartFrame<'_>) -> bool {
            self.harness
                .with_cx(|cx| self.chart.sync(cx, frame))
                .expect("the chart syncs")
        }

        /// Syncs one frame and runs the frame that paints it.
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

    /// The candle bodies a canvas holds.
    ///
    /// A body is a full-strength candle-coloured quad exactly `body_width` wide: a wick
    /// is one pixel, a dashed line is the dash length, and a volume bar at rest is the
    /// market's weak wash rather than the candle's own colour.
    fn candle_bodies(
        primitives: &[CanvasPrimitive],
        colors: ChartPalette,
        body_width: f32,
    ) -> Vec<Rect> {
        primitives
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .filter(|primitive| {
                primitive.color == Some(colors.chart.bullish)
                    || primitive.color == Some(colors.chart.bearish)
            })
            .filter_map(|primitive| primitive.bounds)
            .filter(|bounds| (bounds.size.width - body_width).abs() < 0.01)
            .collect()
    }

    /// The full-strength bar quads a canvas holds inside `plot`.
    fn full_strength_bars_in(
        primitives: &[CanvasPrimitive],
        colors: ChartPalette,
        plot: Rect,
    ) -> Vec<Rect> {
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

    /// A configuration with every indicator switched off.
    fn no_indicators() -> CandlestickIndicatorConfig {
        let mut config = CandlestickIndicatorConfig::default();
        for entry in config.entries.iter_mut() {
            entry.enabled = false;
        }
        config
    }

    /// The default configuration with `ids` additionally enabled.
    fn with_indicators(ids: &[IndicatorId]) -> CandlestickIndicatorConfig {
        let mut config = CandlestickIndicatorConfig::default();
        for entry in config.entries.iter_mut() {
            if ids.contains(&entry.id) {
                entry.enabled = true;
            }
        }
        config
    }

    // ── the canvas read points ───────────────────────────────────────────────────

    /// The selector is the node's id and the tests' only handle on it, so a rename has
    /// to fail here rather than silently break every canvas assertion below.
    #[test]
    fn the_canvas_selector_is_the_node_id_the_tests_find() {
        assert_eq!(ChartCanvas::selector(), "CandlestickChart");
    }

    /// A canvas the widget has not built yet has no node, no box and no boxes.
    #[test]
    fn a_fresh_canvas_has_no_node_and_no_boxes() {
        let canvas = ChartCanvas::new();

        assert_eq!(canvas.node(), None);
        assert_eq!(canvas.bounds(), None);
        assert!(canvas.layout().is_none());
        assert_eq!(canvas.layout_width(), None);
    }

    /// A canvas paints in absolute window coordinates: its first primitive clips to the
    /// node's own box, and every bracket it opens it also closes.
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

    /// I3: a frame that would draw the same picture writes nothing, commits nothing and
    /// asks for nothing — the node keeps the very same `Arc<Canvas>`.
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

    /// The atlas lease is part of the decision: a shaped line whose tiles were reused
    /// cannot be drawn, and the frame that follows has to rebuild the canvas. Moving the
    /// raster scale is the reachable way to invalidate the lease from a headless test
    /// (the engine re-rasterises at the new ppem and the old handles go stale).
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

        harness
            .harness
            .app_mut()
            .set_text_scale(2.0)
            .expect("2.0 is a valid raster scale");

        assert!(
            harness.sync(&frame),
            "a dead text lease is a frame that rebuilds"
        );
        harness.harness.drain();
        let after = harness
            .harness
            .canvas_handle(node)
            .expect("the node holds a canvas");
        assert!(
            !Arc::ptr_eq(&before, &after),
            "the canvas was rebuilt with re-shaped text"
        );
        assert!(
            kinds(&harness.primitives(), PrimitiveKind::GlyphRun) > 0,
            "the labels are re-shaped rather than lost"
        );
    }

    /// Colours are resolved from the theme on the frame that draws, never baked at
    /// build time: switching the appearance repaints in the new theme.
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

    /// A frame's indicator polylines are registered geometry; releasing the chart
    /// returns every handle it lent, and takes its own node down.
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

    /// A snapshot with no candle still resolves its boxes and paints the axes' frame;
    /// there is no tick, tag or readout text to paint without a candle.
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

    /// Panning keeps the slots and moves the candles: the axis grid stays in the same
    /// slots and the candle bodies are drawn from the candles of the window showing now.
    ///
    /// The time grid is aligned to absolute candle indices (so the labels do not crawl
    /// while the chart is dragged), which is why the pan here is a whole number of label
    /// strides: a pan that is not aligned moves the grid too, by design.
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
        // The body width is what tells a candle body from a wick, a dashed line and a
        // volume bar, so it is taken from the boxes the frame drew with.
        let layout = harness.layout();
        let plot = layout.price_plot;
        let viewport = ChartViewport::default();
        let range = visible_range(snapshot.candles(), viewport, plot.size.width);
        let body_width = CandleGeometry::new(plot, range.len(), viewport.candle_width).body_width();
        let bodies_before = candle_bodies(&harness.primitives(), colors, body_width);
        let grid_before: Vec<f32> = harness
            .primitives()
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .filter(|primitive| primitive.color == Some(colors.chart.grid))
            .filter_map(|primitive| primitive.bounds)
            .filter(|bounds| bounds.size.width == LINE_WIDTH)
            .map(|bounds| bounds.origin.x)
            .collect();

        // The label stride is what the time grid is aligned to, so a pan of whole
        // strides keeps every tick in the slot it was in.
        let stride = ((70.0_f32 / viewport.candle_width).ceil() as usize).max(1);
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
        let grid_after: Vec<f32> = harness
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

    /// The readout and the legend are painted whether or not the pointer is over the
    /// chart: only the crosshair follows it.
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

    /// The readout is clipped to its own text row, so a label wider than a narrow pane
    /// cannot paint over the legend row below it.
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

    /// The indicator values are cached on the snapshot and the configuration alone, so
    /// panning reads the cache instead of recomputing every line.
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

    /// The readout names the candle under the pointer, read by its absolute index.
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
        let cursor = Point::new(geometry.center_x(slot), plot.origin.y + 20.0);

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

    /// With the pointer away, the readout describes the newest candle the viewport
    /// shows — not the newest candle in the snapshot.
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

    /// The crosshair's bottom tag carries the date of the candle under the pointer.
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
        let cursor = Point::new(geometry.center_x(slot), plot.origin.y + 20.0);

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

    /// A histogram bar is drawn at full strength only under the pointer.
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
        let cursor = Point::new(geometry.center_x(slot), plot.origin.y + 20.0);
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

    /// One pointer lights the same slot in both bar plots: the volume bar and the MACD
    /// histogram bar the crosshair is on are the two bars at full strength.
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
        let cursor = Point::new(geometry.center_x(slot), plot.origin.y + 20.0);
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

    /// A pointer over a y-axis is away from the data: no crosshair is drawn at all.
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
        let cursor = Point::new(
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

    /// Every enabled sub plot gets exactly one box (and its own labelled axis).
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
        let axis_clips: Vec<Rect> = layout
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

    /// A chart with every indicator switched off paints the readout row only: no
    /// legend text is shaped at all.
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

    /// Indicators of different kinds take a legend row each, in catalogue order.
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

    // ── the ported pure tests ────────────────────────────────────────────────────

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
            Rect::new(0.0, 0.0, 320.0, 240.0),
            LayoutInputs {
                longest_label_width: 0.0,
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
        let plot = Rect::new(10.0, 20.0, 20.0, 20.0);
        let mut horizontal = Vec::with_capacity(3);
        let mut vertical = Vec::with_capacity(3);
        for_each_horizontal_dash(25.0, plot, |bounds| horizontal.push(bounds));
        for_each_vertical_dash(15.0, plot, |bounds| vertical.push(bounds));

        assert_eq!(
            horizontal,
            [
                Rect::new(10.0, 25.0, 5.0, 1.0),
                Rect::new(19.0, 25.0, 5.0, 1.0),
                Rect::new(28.0, 25.0, 2.0, 1.0),
            ]
        );
        assert_eq!(
            vertical,
            [
                Rect::new(15.0, 20.0, 1.0, 5.0),
                Rect::new(15.0, 29.0, 1.0, 5.0),
                Rect::new(15.0, 38.0, 1.0, 2.0),
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
        let short = price_axis_values(0.0, 1_000.0, 200.0, tick);
        let tall = price_axis_values(0.0, 1_000.0, 480.0, tick);

        assert!(tall.len() > short.len());
    }

    #[test]
    fn price_axis_values_should_use_compact_nice_steps_for_typical_height() {
        let tick = Tick::from_inner(1).expect("price tick should be positive");
        let values = price_axis_values(0.0, 1_000.0, 360.0, tick);

        assert_eq!(values[1] - values[0], 100.0);
    }

    #[test]
    fn price_axis_values_should_keep_minimum_label_spacing() {
        let tick = Tick::from_inner(1).expect("price tick should be positive");
        let height = 360.0;
        let values = price_axis_values(0.0, 1_000.0, height, tick);

        assert!(values.windows(2).all(|pair| {
            let spacing = (pair[1] - pair[0]) / 1_000.0 * f64::from(height);
            spacing >= f64::from(PRICE_AXIS_MIN_LABEL_SPACING)
        }));
    }

    #[test]
    fn price_axis_values_should_use_one_label_when_plot_is_too_short() {
        let tick = Tick::from_inner(1).expect("price tick should be positive");

        assert_eq!(price_axis_values(0.0, 1_000.0, 20.0, tick).len(), 1);
    }

    #[test]
    fn price_axis_values_should_not_repeat_labels_for_coarse_price_ticks() {
        let tick = Tick::from_inner(250).expect("price tick should be positive");
        let labels = price_axis_values(0.0, 1_000.0, 480.0, tick)
            .into_iter()
            .map(|value| format_price(value, tick))
            .collect::<Vec<_>>();

        assert!(labels.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn readout_bounds_should_not_exceed_narrow_price_plot() {
        let plot = Rect::new(10.0, 20.0, 6.0, 10.0);
        let readout = readout_bounds(plot);

        assert_eq!((readout.size.width, readout.size.height), (0.0, 2.0));
    }

    /// The readout's box is the price plot's first text row: it starts one row inset
    /// from the plot's top and is exactly one text row tall.
    #[test]
    fn the_readout_should_take_the_first_text_row() {
        let plot = Rect::new(0.0, 120.0, 320.0, 240.0);
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

    /// The readout's box shrinks when the plot cannot hold a row, so a degenerate plot
    /// draws no text at all rather than into its neighbour.
    #[test]
    fn the_readout_should_have_no_room_in_a_plot_shorter_than_one_row() {
        let plot = |height: f32| Rect::new(0.0, 0.0, 320.0, height);

        assert_eq!(
            readout_bounds(plot(TEXT_ROW_INSET)).size.height,
            0.0,
            "a plot exactly one inset tall has no row to give"
        );
        assert_eq!(
            readout_bounds(plot(10.0)).size.height,
            2.0,
            "a plot 10px tall gives what is left after the inset, clipped to one row"
        );
        assert_eq!(
            readout_bounds(plot(240.0)).size.height,
            TEXT_ROW_HEIGHT,
            "a plot with room gives exactly one whole row"
        );
    }

    /// Both text rows fit a price plot of exactly `MIN_PLOT_HEIGHT`.
    #[test]
    fn the_two_text_rows_should_fit_a_price_plot_at_the_reserved_height() {
        let floor = crate::chart::geometry::MIN_PLOT_HEIGHT;
        let plot = Rect::new(0.0, 0.0, 320.0, floor);

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
        assert_eq!(axis_text_width(4.0, AXIS_TICK_TEXT_INSET), 0.0);
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
            300.0,
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

    /// The plot is scaled by traded sizes only: a monetary amount is another quantity,
    /// and a chart that plotted it would be drawing two units on one axis.
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
        let plot = Rect::new(10.0, 20.0, 400.0, 100.0);

        assert_eq!(y_for_volume(0.0, 100.0, plot), plot.max().y);
    }

    #[test]
    fn maximum_volume_should_map_to_plot_top() {
        let plot = Rect::new(10.0, 20.0, 400.0, 100.0);

        assert_eq!(y_for_volume(100.0, 100.0, plot), plot.origin.y);
    }

    #[test]
    fn volume_hover_midpoint_should_map_to_half_the_upper_bound() {
        let plot = Rect::new(10.0, 20.0, 400.0, 100.0);
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

    /// A canvas whose inputs are all unchanged still has to be repainted when a shaped line
    /// it drew lost its atlas lease: the engine drops a stale run at encode time, so reusing
    /// the canvas would draw the label as nothing and keep doing so.
    #[test]
    fn a_canvas_with_a_stale_text_lease_should_not_match() {
        let snapshot = one_candle_snapshot();
        let indicators = CandlestickIndicatorConfig::default();
        let countdown = SmolStr::new_static("00:00");
        let viewport = ChartViewport::default();
        let frame = frame_of(&snapshot, viewport, &indicators, &countdown);
        let key = PaintedKey {
            bounds: Rect::new(0.0, 0.0, 320.0, 240.0),
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

    /// The canvas on screen is reused only when every input that decides what it draws
    /// is unchanged. Each case below is one way a frame can differ.
    #[test]
    fn a_painted_key_should_reject_every_changed_drawing_input() {
        let snapshot = one_candle_snapshot();
        let bounds = Rect::new(0.0, 0.0, 320.0, 240.0);
        let viewport = ChartViewport {
            candle_width: 9.0,
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
                candle_width: 10.0,
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
                bounds: Rect::new(0.0, 0.0, 640.0, 480.0),
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
                cursor: Some(Point::new(1.0, 2.0)),
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

        // The three inputs that are compared beside the key, and the atlas lease.
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

    /// A two-point run over the left edge proves both halves of the mesh contract: the
    /// extra candle is drawn, and a `None` never gets a segment fabricated across it.
    #[test]
    fn an_indicator_line_mesh_should_break_at_a_none_and_take_one_candle_from_the_left() {
        let plot = Rect::new(0.0, 0.0, 100.0, 100.0);
        let geometry = CandleGeometry::new(plot, 2, 10.0);
        let y_for = |value: f64| value as f32;

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

    /// Passing a relative slice with an absolute range has no consistency constraint,
    /// so the contract is checked at the entry.
    #[test]
    #[should_panic(expected = "the range indexes the whole snapshot")]
    fn an_indicator_line_mesh_should_reject_a_relative_slice() {
        let plot = Rect::new(0.0, 0.0, 100.0, 100.0);
        let geometry = CandleGeometry::new(plot, 2, 10.0);

        let _ = LineMesh::polyline(&[Some(1.0), Some(2.0)], 1..3, geometry, |value| {
            value as f32
        });
    }

    /// The extra candle an indicator line draws is one slot left of the first visible
    /// one, which `center_x` cannot address because that slot is outside `visible_len`.
    /// A chart with no visible candle must still answer it.
    #[test]
    fn an_indicator_line_mesh_should_take_one_candle_width_left_of_the_first_slot() {
        let plot = Rect::new(0.0, 0.0, 100.0, 100.0);
        let geometry = CandleGeometry::new(plot, 2, 10.0);

        assert_eq!(
            geometry.center_x_left_of_first(1),
            geometry.center_x(0) - 10.0
        );
        assert_eq!(geometry.center_x_left_of_first(1), 75.0);

        let empty = CandleGeometry::new(plot, 0, 10.0);
        assert_eq!(empty.center_x_left_of_first(1), empty.center_x(0) - 10.0);

        let mesh = LineMesh::polyline(&[Some(1.0), Some(2.0)], 1..2, geometry, |value| {
            value as f32
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

    /// A range whose start is past its end is not a visible window; the debug assertion
    /// names it instead of silently producing an empty mesh.
    #[test]
    #[should_panic(expected = "range.start <= range.end")]
    fn an_indicator_line_mesh_should_reject_a_reversed_range() {
        let plot = Rect::new(0.0, 0.0, 100.0, 100.0);
        let geometry = CandleGeometry::new(plot, 2, 10.0);
        // Built from variables so the lint that flags a literal `2..1` does not fire:
        // the reversed range is the input this test is about.
        let start = 2;
        let end = 1;

        let _ = LineMesh::polyline(&[Some(1.0), Some(2.0)], start..end, geometry, |value| {
            value as f32
        });
    }

    /// The padding is computed once, from the range that already contains the indicator
    /// values; padding the candle range first and merging afterwards would leave the
    /// line outside the box it was padded for.
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

    /// The range indexes the whole snapshot, so a range that reaches past it means the
    /// caller passed a relative slice; that is a contract violation, not an empty chart.
    #[test]
    #[should_panic(expected = "the range indexes the whole snapshot")]
    fn price_bounds_with_indicators_should_reject_a_relative_slice() {
        let candles = [candle_with_prices(1, 100, 110, 90, 100)];

        let _ = price_bounds_with_indicators(&candles, 0..2, None, &[]);
    }

    /// Only main-chart lines move the price range; a sub-plot value (here a MACD
    /// histogram far outside the candles) must leave it alone
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

    /// The axis is sized from these candidates, so a main-chart line that moved the
    /// price range must add its own label to the set; otherwise the axis could be too
    /// narrow for the tick the plot draws.
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
        let with_indicator = axis_label_candidates(&candles, 0..1, None, tick, 300.0, &series, &[]);
        let baseline = axis_label_candidates(&candles, 0..1, None, tick, 300.0, &[], &[]);

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
            Rect::new(0.0, 0.0, 320.0, 240.0),
            LayoutInputs {
                longest_label_width: 0.0,
                sub_plot_count: 1,
            },
        );
        let geometry = CandleGeometry::new(layout.price_plot, 10, 9.0);
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
        let cursor = Point::new(geometry.center_x(3), layout.price_plot.origin.y + 5.0);

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

    /// The readout is drawn whether or not the pointer is over the chart, so its index
    /// has to be defined when there is no cursor at all: it is the newest visible
    /// candle — the rightmost one of the current viewport, not the newest candle in the
    /// snapshot and not the first visible one.
    #[test]
    fn the_readout_should_describe_the_newest_visible_candle_without_a_cursor() {
        let snapshot = snapshot_with_candles(200);
        let layout = ChartLayout::new(
            Rect::new(0.0, 0.0, 320.0, 240.0),
            LayoutInputs {
                longest_label_width: 0.0,
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

        // Another range with a different start and length, so the answer tracks the
        // viewport rather than a position within a ten-wide window.
        let moved = visible_over(&snapshot, 150..175, &layout);
        assert_eq!(
            readout_candle_index(None, &layout, &moved),
            174,
            "a viewport of 150..175 ends at candle 174"
        );

        // A pointer that is over the axes rather than over data is away from the chart
        // too: it names no candle, so the fallback stands.
        let over_the_axis = Point::new(
            layout.price_axis.origin.x + layout.price_axis.size.width / 2.0,
            layout.price_axis.origin.y + layout.price_axis.size.height / 2.0,
        );
        assert_eq!(
            readout_candle_index(Some(over_the_axis), &layout, &visible),
            19,
            "a pointer over the axis falls back to the newest visible candle"
        );
    }

    /// The fallback follows the viewport: after a pan the newest visible candle is
    /// another one, and the readout must read that candle rather than the one the chart
    /// happened to be showing before.
    #[test]
    fn the_readout_fallback_should_follow_the_viewport_after_a_pan() {
        let snapshot = snapshot_with_candles(200);
        let layout = ChartLayout::new(
            Rect::new(0.0, 0.0, 320.0, 240.0),
            LayoutInputs {
                longest_label_width: 0.0,
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

    /// A chart with every sub-plot indicator switched off has no sub-plot boxes at all:
    /// the price plot takes the content and the time axis sits directly under it.
    #[test]
    fn a_chart_without_enabled_sub_plots_should_have_no_sub_plot_boxes() {
        let layout = ChartLayout::new(
            Rect::new(0.0, 0.0, 320.0, 240.0),
            LayoutInputs {
                longest_label_width: 50.0,
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
            Rect::new(0.0, 0.0, 320.0, 240.0),
            LayoutInputs {
                longest_label_width: 50.0,
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

    /// A pane too short for sub plots keeps the price plot's floor and draws no sub-plot
    /// box at all. Nothing may be prepared for boxes that do not exist: the painter
    /// indexes the prepared list by the layout's boxes.
    #[test]
    fn a_pane_too_short_for_sub_plots_should_prepare_none() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            Rect::new(0.0, 0.0, 320.0, 60.0),
            LayoutInputs {
                longest_label_width: 50.0,
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

    /// Only a VOL first sub plot owns the volume bars. When MACD is the only enabled
    /// sub plot the bars must not be drawn into its box.
    #[test]
    fn a_first_sub_plot_that_is_not_volume_should_not_prepare_volume_bars() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            Rect::new(0.0, 0.0, 320.0, 240.0),
            LayoutInputs {
                longest_label_width: 50.0,
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

    /// VOL is the default sub plot: it prepares one box and its bars.
    #[test]
    fn the_default_volume_sub_plot_should_prepare_its_bars() {
        let snapshot = snapshot_with_candles(10);
        let layout = ChartLayout::new(
            Rect::new(0.0, 0.0, 320.0, 240.0),
            LayoutInputs {
                longest_label_width: 50.0,
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

    /// MACD spans are symmetric around zero and include the histogram, which is a line
    /// like the other two.
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

    /// A MACD bar takes the colour of the direction it points and the strength the
    /// pointer gives it: non-negative is bullish (a zero bar included) and negative is
    /// bearish, at the resting wash or — for the bar under the pointer — at full
    /// strength. The four colours are supplied by the test so it needs no theme.
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

    /// The two strengths a bar is drawn at, and the direction each one belongs to, are
    /// the market roles themselves — in both appearances, because the wash is an alpha.
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

    /// J can leave the 0..100 domain on either side, and the range has to follow it
    /// rather than clamp it at zero (design D11).
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

    /// All-None KDJ data falls back to the percentage domain rather than to an empty
    /// range.
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

    /// The range is derived from the visible window, so panning to a calmer or wilder
    /// stretch changes it even though the values did not move.
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
            sub_axis_values(IndicatorId::Macd, value_range, 100.0),
            [-5.0, 0.0, 5.0]
        );
    }

    /// An all-zero MACD collapses to a 1e-9 span; the axis must still produce its three
    /// labels instead of panicking on a zero range.
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
            sub_axis_values(IndicatorId::Macd, value_range, 100.0).len(),
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
            sub_axis_values(IndicatorId::Rsi, domain, 40.0),
            [30.0, 50.0, 70.0]
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Rsi, domain, 120.0),
            [0.0, 30.0, 50.0, 70.0, 100.0]
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Kdj, domain, 40.0),
            [0.0, 50.0, 100.0]
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Kdj, domain, 120.0),
            [0.0, 20.0, 80.0, 100.0]
        );
    }

    /// The first-round regression filled a guide's dashed line with a solid grid line.
    /// The fix has to keep the guide labelled *and* keep its grid line out; both halves
    /// are pinned here.
    #[test]
    fn sub_plot_axis_should_label_guides_without_drawing_their_grid_line() {
        let domain = ValueRange {
            lowest: 0.0,
            highest: 100.0,
        };

        let tall_rsi = sub_plot_axis(IndicatorId::Rsi, domain, 120.0, &[30.0, 70.0]);
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

        // The compact axis labels only 30/50/70, but 30 and 70 are still guides, so
        // only 50 may get a solid grid line.
        let compact_rsi = sub_plot_axis(IndicatorId::Rsi, domain, 40.0, &[30.0, 70.0]);
        assert_eq!(compact_rsi.labels, [30.0, 50.0, 70.0]);
        assert_eq!(compact_rsi.grid, [50.0]);

        let tall_kdj = sub_plot_axis(IndicatorId::Kdj, domain, 120.0, &[20.0, 80.0]);
        assert_eq!(tall_kdj.labels, [0.0, 20.0, 80.0, 100.0]);
        assert_eq!(tall_kdj.grid, [0.0, 100.0]);

        let macd = sub_plot_axis(
            IndicatorId::Macd,
            ValueRange {
                lowest: -5.0,
                highest: 5.0,
            },
            120.0,
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

    /// Five labels need five 14px rows: the tall mode must still be used at exactly
    /// 70px and drop to three just below that.
    #[test]
    fn sub_plot_axis_should_switch_mode_at_five_label_rows() {
        let domain = ValueRange {
            lowest: 0.0,
            highest: 100.0,
        };

        assert_eq!(
            sub_axis_values(IndicatorId::Rsi, domain, 70.0).len(),
            5,
            "five 14px label rows fit exactly in 70px"
        );
        assert_eq!(
            sub_axis_values(IndicatorId::Rsi, domain, 69.9).len(),
            3,
            "one pixel less cannot fit the five label rows"
        );
    }

    /// A KDJ range that leaves the domain on both sides has six candidates; the fixed
    /// endpoint closer to its extreme is dropped so at most five remain.
    #[test]
    fn kdj_axis_should_drop_the_fixed_endpoint_nearest_the_dynamic_extreme() {
        let value_range = ValueRange {
            lowest: -18.0,
            highest: 124.0,
        };

        let values = sub_axis_values(IndicatorId::Kdj, value_range, 120.0);

        assert!((3..=5).contains(&values.len()), "got {values:?}");
        assert_eq!(values, [-18.0, 20.0, 80.0, 100.0, 124.0]);
    }

    /// An in-domain series still gets the 5% padding, but the padding is not a dynamic
    /// extremum: labelling it would stack `-5` / `105` on top of `0` / `100`.
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
            sub_axis_values(IndicatorId::Kdj, value_range, 120.0),
            [0.0, 20.0, 80.0, 100.0]
        );
    }

    /// The signed MACD tick has to be able to widen the shared axis; the candidate set
    /// is where that width comes from.
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
            300.0,
            &series,
            &[IndicatorId::Macd],
        );

        assert!(
            labels.contains(&format_signed_price(-inner, tick)),
            "negative MACD candidates were {labels:?}"
        );
        assert!(labels.contains(&format_signed_price(inner, tick)));
        let baseline = axis_label_candidates(&candles, 0..1, None, tick, 300.0, &[], &[]);
        assert!(
            !baseline.contains(&format_signed_price(-inner, tick)),
            "without the MACD sub plot the signed tick is not a candidate"
        );
    }

    /// The percentage reference lines are axis labels too: the candidate set feeds the
    /// shared axis width, so it must contain the 30/70 and 20/80 ticks even though their
    /// solid grid lines are skipped.
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
            300.0,
            &series,
            &[IndicatorId::Rsi, IndicatorId::Kdj],
        );
        for value in [0.0, 30.0, 50.0, 70.0, 100.0, 20.0, 80.0] {
            assert!(
                labels.contains(&format_percentage_tick(value)),
                "the tick {value} is missing from {labels:?}"
            );
        }

        let baseline = axis_label_candidates(&candles, 0..1, None, tick, 300.0, &[], &[]);
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
            Color::WHITE,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "VOL 1.234K");
    }

    /// The sub-plot legend indexes the values by absolute candle, and only shows a line
    /// once that line is readable there.
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

    /// RSI's line labels already carry the formula, so the legend must not repeat it as
    /// `RSI RSI6`; KDJ reads its three lines in percent.
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

    /// A legend entry is drawn in the colour of the line it describes.
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

        // KDJ's first entry is the formula, so the line's entry is the second.
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

    /// A J that is a hair below zero has no meaningful negative magnitude, but `{:.1}`
    /// would print it as `-0.0`; the readout must round to `0.0`.
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
