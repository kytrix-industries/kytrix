//! One wallet's last six hours of fills, drawn as an axis with a marker per bucket.
//!
//! # Why it is painted rather than laid out
//!
//! A bucket can hold dozens of fills and the axis is rebuilt whenever the window changes, so
//! the markers are painted by one canvas instead of being built as elements: a hundred
//! absolutely positioned divs would be a hundred elements the layout engine has to place on
//! every frame, and none of them would be cheaper to draw than a rectangle.
//!
//! # What is cached, and against what
//!
//! The geometry of a timeline is a pure function of three things — the buckets, the plot width
//! and the width of the start label — so it is cached against exactly those three and thrown
//! away when one of them changes. Shaped text is cached the same way, against the text, its
//! colour and its size, which is what keeps the ten-second refresh from re-shaping the same
//! labels.
//!
//! # Why the marker surface is a hover card rather than a tooltip
//!
//! A bucket's fills are a list a reader scrolls, so the surface that shows them has to stay
//! open while the pointer is inside it. A tooltip closes as soon as the pointer leaves its
//! trigger, which is why this module builds its own hover card on the popup primitive: the
//! open and close delays belong to the primitive, and the rows are a uniform list with a
//! scrollbar of their own.

use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

use ahash::AHashMap;
use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    scroll_bar::Scrollbar,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    Anchor, AnyElement, App, Bounds, ContentMask, ElementId, Hsla, InteractiveElement, IntoElement,
    ParentElement, Pixels, ScrollHandle, ShapedLine, SharedString, StatefulInteractiveElement,
    Styled, TextAlign, TextRun, TextStyle, UniformListScrollHandle, Window, base::HoverCard,
    canvas, div, fill, point, prelude::FluentBuilder, px, rems, size, transparent_black,
    uniform_list,
};
use i18n::t;
use market_participants_connector::account::{
    domain::HyperliquidUserFill, shared::HyperliquidFillDirection,
};
use market_participants_model::{
    decimal::SignedQuantity,
    instrument::InstrumentKind,
    listing::route::VenueListingKey,
    market::{
        price::MarketPrice,
        units::{MarketQuantity, Side},
    },
};
use smol_str::{SmolStr, format_smolstr};
use widget::time_zone::data_time_zone;

use super::model::{
    AgeBucket, DisplayPrecision, TimelineSnapshot, absolute_time, age_bucket, now_millis,
};

/// The height of one wallet's timeline, its header included.
pub(super) const HEIGHT: f32 = 136.0;
/// The narrowest a timeline is drawn at.
///
/// Below this the markers of six hours would overlap, so a narrower viewport scrolls instead
/// of compressing the axis.
pub(super) const MIN_CONTENT_WIDTH: f32 = 560.0;
/// The window a timeline covers, which is also the window one request asks for.
pub(super) const WINDOW_MILLIS: i64 = 6 * 60 * 60 * 1_000;
/// The horizontal padding around a timeline inside its scrolling viewport.
pub(super) const HORIZONTAL_PADDING: f32 = 16.0;

/// The width one bucket occupies, which decides how many of them there are.
const BUCKET_WIDTH: f32 = 24.0;
/// The gap between the axis and the label at either end of it.
const AXIS_LABEL_GAP: f32 = 8.0;
/// How much room the scrollbar under a timeline takes.
const SCROLLBAR_THICKNESS: Pixels = px(4.0);
const HEADER_HEIGHT: f32 = 20.0;
const BODY_HEIGHT: f32 = HEIGHT - HEADER_HEIGHT;
const AXIS_Y: f32 = 58.0;
const AXIS_LINE_HEIGHT: f32 = 1.0;
const AXIS_TICK_HEIGHT: f32 = 8.0;
const AXIS_FONT_SIZE_REMS: f32 = 0.75;
const MARKER_MIN_SIZE: f32 = 18.0;
const MARKER_MAX_SIZE: f32 = 24.0;
const MARKER_GAP: f32 = 6.0;
const MARKER_AXIS_GAP: f32 = 4.0;
const MARKER_RADIUS_RATIO: f32 = 0.22;
const MARKER_BORDER_WIDTH: f32 = 1.0;
const MARKER_FONT_RATIO: f32 = 0.68;
const CONNECTOR_WIDTH: f32 = 1.0;
const TOOLTIP_WIDTH: f32 = 320.0;
const TOOLTIP_ROW_HEIGHT: f32 = 32.0;
const TOOLTIP_VISIBLE_ROWS: usize = 6;
const TEXT_CACHE_CAPACITY: usize = 256;
/// How long the pointer rests on a marker before its fills are shown.
const HOVER_OPEN_DELAY: Duration = Duration::from_millis(150);
/// How long the pointer may be outside the card before it closes, which is what lets it cross
/// the gap between the marker and the card.
const HOVER_CLOSE_DELAY: Duration = Duration::from_millis(250);

type TooltipRowsByBucket = AHashMap<(Side, usize), Arc<[TooltipRow]>>;
type SharedTooltipRows = Arc<TooltipRowsByBucket>;
/// Called when the viewport a timeline is drawn in changes, with that viewport's width and the
/// width of the axis inside it.
pub(super) type OnMeasure = Rc<dyn Fn(f32, f32, &mut App)>;

/// One side of one bucket: the fills that were traded in that slot of the axis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TimelineBucket {
    pub side: Side,
    pub index: usize,
    pub fill_indices: Vec<usize>,
}

/// What a fill did to the position it landed on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FillDirection {
    Long,
    Short,
    LongToShort,
    ShortToLong,
    Liquidated,
    AutoDeleveraged,
    Unknown,
}

/// One fill of a bucket, as the card shows it.
///
/// A row carries the units it was read with rather than text: what a quantity counts and what
/// its price is quoted in are properties of the listing, so a card that is drawn while another
/// reading arrives still names its own units.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TooltipRow {
    pub timestamp: i64,
    pub listing: VenueListingKey,
    pub kind: InstrumentKind,
    pub quantity: MarketQuantity,
    pub price: MarketPrice,
    pub direction: FillDirection,
}

impl TooltipRow {
    /// The quantity of this row, named in the unit its listing publishes it in.
    pub(super) fn quantity_text(&self, precision: &DisplayPrecision, cx: &App) -> SmolStr {
        let value = precision
            .format_listing_quantity(&self.listing, self.quantity.value)
            .unwrap_or_else(|_| SmolStr::new(self.quantity.value.to_string().as_str()));
        match precision.quantity_unit_label(&self.quantity.unit, cx) {
            Some(unit) => format_smolstr!("{value} {unit}"),
            None => value,
        }
    }

    /// The price of this row, in the quote asset its listing publishes it against.
    pub(super) fn price_text(&self, precision: &DisplayPrecision, cx: &App) -> SmolStr {
        let value = precision
            .format_listing_price(&self.listing, self.price.value)
            .unwrap_or_else(|_| SmolStr::new(self.price.value.to_string().as_str()));
        format_smolstr!(
            "{value} {}",
            precision.asset_label(&self.price.quote_asset, cx)
        )
    }
}

/// One wallet's timeline: what it is showing, and the geometry it was last drawn at.
///
/// The scroll handles are kept per bucket rather than per frame, which is what keeps a card
/// that is open where the reader left it across the ten-second refresh.
#[derive(Clone)]
pub(super) struct TimelineEntry {
    snapshot: Option<Arc<TimelineSnapshot>>,
    buckets: Arc<[TimelineBucket]>,
    tooltip_scrolls: Rc<AHashMap<(Side, usize), UniformListScrollHandle>>,
    scroll: ScrollHandle,
    width: f32,
    plot_width: f32,
    paint_cache: Rc<RefCell<TimelinePaintCache>>,
}

impl Default for TimelineEntry {
    fn default() -> Self {
        Self {
            snapshot: None,
            buckets: Arc::from([]),
            tooltip_scrolls: Rc::new(AHashMap::new()),
            scroll: ScrollHandle::default(),
            width: MIN_CONTENT_WIDTH,
            plot_width: MIN_CONTENT_WIDTH,
            paint_cache: Rc::new(RefCell::new(TimelinePaintCache::default())),
        }
    }
}

impl TimelineEntry {
    /// Takes a published snapshot, and says whether anything a frame draws changed.
    ///
    /// The ten-second cadence publishes the same window whenever a wallet has not traded, so an
    /// unchanged snapshot is recognised by identity rather than by comparing fills.
    pub(super) fn replace_snapshot(&mut self, snapshot: &Arc<TimelineSnapshot>) -> bool {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, snapshot))
        {
            return false;
        }
        self.sync_buckets(snapshot, self.plot_width);
        self.snapshot = Some(snapshot.clone());
        true
    }

    /// Records the width the timeline was laid out at, and rebuckets if it changed.
    pub(super) fn set_dimensions(&mut self, width: f32, plot_width: f32) -> bool {
        let width = finite_width(width);
        let plot_width = finite_width(plot_width);
        let bucket_count_changed = bucket_count(self.plot_width) != bucket_count(plot_width);
        if !bucket_count_changed
            && (self.width - width).abs() <= 0.5
            && (self.plot_width - plot_width).abs() <= 0.5
        {
            return false;
        }
        self.width = width;
        self.plot_width = plot_width;
        if let Some(snapshot) = self.snapshot.clone() {
            self.sync_buckets(&snapshot, plot_width);
        }
        true
    }

    fn sync_buckets(&mut self, snapshot: &TimelineSnapshot, plot_width: f32) {
        let mut paint_cache = self.paint_cache.borrow_mut();
        sync_buckets(
            snapshot,
            plot_width,
            &mut self.buckets,
            &mut self.tooltip_scrolls,
            &mut paint_cache,
        );
    }
}

/// What one wallet's row needs besides its own geometry.
pub(super) struct TimelineRowArgs {
    /// The precision the wallet's own overview was valued at, when it has one.
    pub precision: Option<DisplayPrecision>,
    /// The neutral text to show while there is nothing to draw.
    pub loading: SharedString,
    /// Why this wallet's timeline failed, once it has.
    pub failure: Option<SharedString>,
    /// The control that asks for this wallet's timeline again.
    pub retry: Option<AnyElement>,
    pub on_measure: OnMeasure,
}

/// One marker's place on the axis, and the connector that joins it to the axis.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MarkerGeometry {
    side: Side,
    bucket_index: usize,
    bounds: Bounds<Pixels>,
    connector: Bounds<Pixels>,
}

/// Where everything on one timeline is drawn.
#[derive(Debug, PartialEq)]
struct TimelineGeometry {
    axis: Bounds<Pixels>,
    start_tick: Bounds<Pixels>,
    end_tick: Bounds<Pixels>,
    markers: Arc<[MarkerGeometry]>,
    marker_size: Pixels,
}

impl TimelineGeometry {
    fn new(buckets: &[TimelineBucket], plot_width: f32, start_label_width: f32) -> Self {
        let plot_width = finite_width(plot_width);
        let bucket_count = bucket_count(plot_width);
        let slot_width = plot_width / bucket_count as f32;
        let marker_size = marker_size(slot_width);
        let axis_left = start_label_width.max(0.0) + AXIS_LABEL_GAP;
        let axis_y = AXIS_Y;
        let axis = Bounds::new(
            point(px(axis_left), px(axis_y)),
            size(px(plot_width), px(AXIS_LINE_HEIGHT)),
        );
        let start_tick = Bounds::new(
            point(px(axis_left), px(axis_y - AXIS_TICK_HEIGHT / 2.0)),
            size(px(AXIS_LINE_HEIGHT), px(AXIS_TICK_HEIGHT)),
        );
        let end_tick = Bounds::new(
            point(
                px(axis_left + plot_width - AXIS_LINE_HEIGHT),
                px(axis_y - AXIS_TICK_HEIGHT / 2.0),
            ),
            size(px(AXIS_LINE_HEIGHT), px(AXIS_TICK_HEIGHT)),
        );

        let mut markers = Vec::with_capacity(buckets.len());
        for bucket in buckets {
            if bucket.index >= bucket_count {
                continue;
            }
            let center_x = axis_left + (bucket.index as f32 + 0.5) * slot_width;
            let left = (center_x - marker_size / 2.0)
                .max(axis_left)
                .min((axis_left + plot_width - marker_size).max(axis_left));
            let (top, connector_top, connector_height) = match bucket.side {
                Side::Buy => {
                    let bottom = axis_y - MARKER_AXIS_GAP;
                    (
                        bottom - marker_size,
                        bottom,
                        (axis_y - bottom).max(AXIS_LINE_HEIGHT),
                    )
                }
                Side::Sell => {
                    let top = axis_y + AXIS_LINE_HEIGHT + MARKER_AXIS_GAP;
                    (
                        top,
                        axis_y + AXIS_LINE_HEIGHT,
                        (top - (axis_y + AXIS_LINE_HEIGHT)).max(AXIS_LINE_HEIGHT),
                    )
                }
            };
            let marker_bounds = Bounds::new(
                point(px(left), px(top)),
                size(px(marker_size), px(marker_size)),
            );
            let connector = Bounds::new(
                point(
                    px(center_x - CONNECTOR_WIDTH / 2.0),
                    px(connector_top.min(top + marker_size)),
                ),
                size(px(CONNECTOR_WIDTH), px(connector_height)),
            );
            markers.push(MarkerGeometry {
                side: bucket.side,
                bucket_index: bucket.index,
                bounds: marker_bounds,
                connector,
            });
        }

        Self {
            axis,
            start_tick,
            end_tick,
            markers: markers.into(),
            marker_size: px(marker_size),
        }
    }
}

fn marker_size(slot_width: f32) -> f32 {
    if !slot_width.is_finite() || slot_width <= 0.0 {
        return 1.0;
    }
    let available = (slot_width - MARKER_GAP).max(1.0);
    available
        .min(MARKER_MAX_SIZE)
        .max(MARKER_MIN_SIZE.min(available))
}

fn finite_width(width: f32) -> f32 {
    if width.is_finite() && width > 0.0 {
        width
    } else {
        1.0
    }
}

/// One shaped line, kept until the text, colour or size it was shaped for changes.
#[derive(Clone)]
struct CachedTextLine {
    text: SharedString,
    color: Hsla,
    font_size: Pixels,
    line: Arc<ShapedLine>,
}

impl CachedTextLine {
    fn matches(&self, text: &SharedString, color: Hsla, font_size: Pixels) -> bool {
        self.text == *text && self.color == color && self.font_size == font_size
    }
}

/// The axis labels one frame is drawn with, and how wide they came out.
struct AxisLabelMetrics {
    labels: (SharedString, SharedString),
    text_style: TextStyle,
    font_size: Pixels,
    widths: (f32, f32),
}

struct GeometryCacheEntry {
    buckets: Arc<[TimelineBucket]>,
    plot_width: f32,
    start_label_width: f32,
    geometry: Arc<TimelineGeometry>,
}

struct TooltipRowsCacheEntry {
    fills: Arc<[HyperliquidUserFill]>,
    buckets: Arc<[TimelineBucket]>,
    rows: SharedTooltipRows,
}

/// Everything one timeline remembers between frames.
///
/// It is bounded: one geometry, one set of card rows, and a fixed number of shaped lines.
pub(super) struct TimelinePaintCache {
    geometry: Option<GeometryCacheEntry>,
    tooltip_rows: Option<TooltipRowsCacheEntry>,
    axis_labels: Option<AxisLabelMetrics>,
    text_style: Option<TextStyle>,
    text_lines: Vec<CachedTextLine>,
}

impl Default for TimelinePaintCache {
    fn default() -> Self {
        Self {
            geometry: None,
            tooltip_rows: None,
            axis_labels: None,
            text_style: None,
            text_lines: Vec::with_capacity(TEXT_CACHE_CAPACITY),
        }
    }
}

impl TimelinePaintCache {
    fn clear_geometry(&mut self) {
        self.geometry = None;
    }

    fn geometry(
        &mut self,
        buckets: &Arc<[TimelineBucket]>,
        plot_width: f32,
        start_label_width: f32,
    ) -> Arc<TimelineGeometry> {
        let plot_width = finite_width(plot_width);
        let matches = self.geometry.as_ref().is_some_and(|entry| {
            Arc::ptr_eq(&entry.buckets, buckets)
                && entry.plot_width.to_bits() == plot_width.to_bits()
                && entry.start_label_width.to_bits() == start_label_width.to_bits()
        });
        if matches && let Some(entry) = self.geometry.as_ref() {
            return entry.geometry.clone();
        }

        let geometry = Arc::new(TimelineGeometry::new(
            buckets,
            plot_width,
            start_label_width,
        ));
        self.geometry = Some(GeometryCacheEntry {
            buckets: buckets.clone(),
            plot_width,
            start_label_width,
            geometry: geometry.clone(),
        });
        geometry
    }

    fn tooltip_rows(
        &mut self,
        snapshot: &TimelineSnapshot,
        buckets: &Arc<[TimelineBucket]>,
        precision: Option<&DisplayPrecision>,
    ) -> SharedTooltipRows {
        let matches = self.tooltip_rows.as_ref().is_some_and(|entry| {
            Arc::ptr_eq(&entry.fills, &snapshot.fills) && Arc::ptr_eq(&entry.buckets, buckets)
        });
        if matches && let Some(entry) = self.tooltip_rows.as_ref() {
            return entry.rows.clone();
        }

        let rows = buckets
            .iter()
            .map(|bucket| {
                (
                    (bucket.side, bucket.index),
                    tooltip_rows(snapshot, bucket, precision).into(),
                )
            })
            .collect::<TooltipRowsByBucket>();
        let rows: SharedTooltipRows = Arc::new(rows);
        self.tooltip_rows = Some(TooltipRowsCacheEntry {
            fills: snapshot.fills.clone(),
            buckets: buckets.clone(),
            rows: rows.clone(),
        });
        rows
    }

    /// The width of the two axis labels, shaped once per text style and language.
    fn axis_label_widths(
        &mut self,
        labels: (SharedString, SharedString),
        window: &Window,
    ) -> (f32, f32) {
        let font_size = rems(AXIS_FONT_SIZE_REMS).to_pixels(window.rem_size());
        let text_style = window.text_style();
        let matches = self.axis_labels.as_ref().is_some_and(|entry| {
            entry.labels == labels && entry.text_style == text_style && entry.font_size == font_size
        });
        if matches && let Some(entry) = self.axis_labels.as_ref() {
            return entry.widths;
        }

        let widths = (
            axis_label_width(&labels.0, font_size, &text_style, window),
            axis_label_width(&labels.1, font_size, &text_style, window),
        );
        self.axis_labels = Some(AxisLabelMetrics {
            labels,
            text_style,
            font_size,
            widths,
        });
        widths
    }

    /// The width of the axis inside a viewport of `total_width`.
    pub(super) fn plot_width(
        &mut self,
        total_width: f32,
        labels: (SharedString, SharedString),
        window: &Window,
    ) -> f32 {
        let (start_width, end_width) = self.axis_label_widths(labels, window);
        axis_line_width(total_width.max(MIN_CONTENT_WIDTH), start_width, end_width)
    }

    /// A shaped line for one label, from the cache when it is already there.
    fn line(
        &mut self,
        text: SharedString,
        color: Hsla,
        font_size: Pixels,
        window: &Window,
    ) -> Arc<ShapedLine> {
        let text_style = window.text_style();
        if self.text_style.as_ref() != Some(&text_style) {
            self.text_style = Some(text_style.clone());
            self.text_lines.clear();
        }
        if let Some(index) = self
            .text_lines
            .iter()
            .position(|entry| entry.matches(&text, color, font_size))
        {
            let entry = self.text_lines.remove(index);
            let line = entry.line.clone();
            self.text_lines.push(entry);
            return line;
        }

        let line = Arc::new(window.text_system().shape_line(
            text.clone(),
            font_size,
            &[TextRun {
                len: text.len(),
                font: text_style.font(),
                color,
                ..Default::default()
            }],
            None,
        ));
        if self.text_lines.len() >= TEXT_CACHE_CAPACITY {
            self.text_lines.remove(0);
        }
        self.text_lines.push(CachedTextLine {
            text,
            color,
            font_size,
            line: line.clone(),
        });
        line
    }
}

/// Rebuilds the visible buckets when a snapshot or a plot width changes.
///
/// Scroll handles of buckets that survive the rebuild are kept, so a card that is open does not
/// jump back to its first row every ten seconds.
fn sync_buckets(
    snapshot: &TimelineSnapshot,
    plot_width: f32,
    buckets: &mut Arc<[TimelineBucket]>,
    tooltip_scrolls: &mut Rc<AHashMap<(Side, usize), UniformListScrollHandle>>,
    paint_cache: &mut TimelinePaintCache,
) {
    let next = build_buckets(snapshot, plot_width.max(1.0));
    if buckets.as_ref() == next.as_slice() {
        return;
    }

    let tooltip_scrolls = Rc::make_mut(tooltip_scrolls);
    tooltip_scrolls.retain(|(side, index), _| {
        next.iter()
            .any(|bucket| bucket.side == *side && bucket.index == *index)
    });
    let additional_scrolls = next.len().saturating_sub(tooltip_scrolls.len());
    tooltip_scrolls.reserve(additional_scrolls);
    for bucket in &next {
        tooltip_scrolls
            .entry((bucket.side, bucket.index))
            .or_default();
    }
    *buckets = next.into();
    paint_cache.clear_geometry();
}

#[derive(Clone)]
struct TimelinePaintState {
    bounds: Bounds<Pixels>,
    geometry: Arc<TimelineGeometry>,
    start_line: Arc<ShapedLine>,
    end_line: Arc<ShapedLine>,
    buy_line: Arc<ShapedLine>,
    sell_line: Arc<ShapedLine>,
}

fn timeline_canvas(
    geometry: Arc<TimelineGeometry>,
    labels: (SharedString, SharedString),
    paint_cache: Rc<RefCell<TimelinePaintCache>>,
) -> impl IntoElement {
    let prepaint_geometry = geometry;
    let (prepaint_start_label, prepaint_end_label) = labels;
    canvas(
        move |bounds, window, cx| {
            let mut cache = paint_cache.borrow_mut();
            let axis_font_size = rems(AXIS_FONT_SIZE_REMS).to_pixels(window.rem_size());
            let marker_font_size =
                (prepaint_geometry.marker_size * MARKER_FONT_RATIO).max(px(10.0));
            // The two ends of the axis are its time ticks, so they are read in the chart's own
            // axis colour (§8.7), not in a text role: a chart's labels are its furniture.
            let axis_label = cx.theme().components.chart.axis.hsla;
            // The letter inside a marker is painted in the window's own background, which reads
            // as a cut-out whatever colour the marker is.
            let marker_text = cx.theme().roles.surface.sunken.hsla;
            TimelinePaintState {
                bounds,
                geometry: prepaint_geometry.clone(),
                start_line: cache.line(
                    prepaint_start_label.clone(),
                    axis_label,
                    axis_font_size,
                    window,
                ),
                end_line: cache.line(
                    prepaint_end_label.clone(),
                    axis_label,
                    axis_font_size,
                    window,
                ),
                buy_line: cache.line(
                    SharedString::new_static("B"),
                    marker_text,
                    marker_font_size,
                    window,
                ),
                sell_line: cache.line(
                    SharedString::new_static("S"),
                    marker_text,
                    marker_font_size,
                    window,
                ),
            }
        },
        move |_, state, window, app| paint_timeline(state, window, app),
    )
    .size_full()
}

fn paint_timeline(state: TimelinePaintState, window: &mut Window, app: &mut App) {
    if state.bounds.size.width <= px(0.0) || state.bounds.size.height <= px(0.0) {
        return;
    }
    // Read out of the style before anything is painted: a shaped line borrows the application
    // mutably, and a colour read after that would borrow it again.
    let (axis, marker_border, buy, sell) = {
        let theme = app.theme();
        (
            theme.components.chart.grid.hsla,
            theme.roles.line.divider.hsla,
            theme.roles.market.up.hsla,
            theme.roles.market.down.hsla,
        )
    };
    let origin = state.bounds.origin;
    let geometry = &state.geometry;
    window.with_content_mask(
        Some(ContentMask {
            bounds: state.bounds,
        }),
        |window| {
            window.paint_quad(fill(window.pixel_snap_bounds(geometry.axis + origin), axis));
            window.paint_quad(fill(
                window.pixel_snap_bounds(geometry.start_tick + origin),
                axis,
            ));
            window.paint_quad(fill(
                window.pixel_snap_bounds(geometry.end_tick + origin),
                axis,
            ));

            // Connectors first, so a bucket whose two sides meet at the axis still shows a
            // continuous join under both markers.
            window.paint_layer(state.bounds, |window| {
                for marker in geometry.markers.iter() {
                    window.paint_quad(fill(
                        window.pixel_snap_bounds(marker.connector + origin),
                        axis,
                    ));
                }
            });
            window.paint_layer(state.bounds, |window| {
                for marker in geometry.markers.iter() {
                    let color = match marker.side {
                        Side::Buy => buy,
                        Side::Sell => sell,
                    };
                    let bounds = window.pixel_snap_bounds(marker.bounds + origin);
                    let radius = (bounds.size.width.as_f32() * MARKER_RADIUS_RATIO)
                        .min(bounds.size.width.as_f32() / 2.0);
                    window.paint_quad(
                        fill(bounds, color)
                            .corner_radii(px(radius))
                            .border_widths(px(MARKER_BORDER_WIDTH))
                            .border_color(marker_border),
                    );
                }
            });

            let marker_line_height = geometry.marker_size;
            for marker in geometry.markers.iter() {
                let bounds = window.pixel_snap_bounds(marker.bounds + origin);
                let line = match marker.side {
                    Side::Buy => &state.buy_line,
                    Side::Sell => &state.sell_line,
                };
                let _ = line.paint(
                    bounds.origin,
                    marker_line_height,
                    TextAlign::Center,
                    Some(bounds.size.width),
                    window,
                    app,
                );
            }

            let axis_font_size = rems(AXIS_FONT_SIZE_REMS).to_pixels(window.rem_size());
            let start_width = state.start_line.width();
            let start_origin = window.pixel_snap_point(point(
                geometry.axis.left() - px(AXIS_LABEL_GAP) - start_width + origin.x,
                geometry.axis.top() - axis_font_size / 2.0 + origin.y,
            ));
            let end_origin = window.pixel_snap_point(point(
                geometry.axis.right() + px(AXIS_LABEL_GAP) + origin.x,
                geometry.axis.top() - axis_font_size / 2.0 + origin.y,
            ));
            let _ = state.start_line.paint(
                start_origin,
                axis_font_size,
                TextAlign::Left,
                None,
                window,
                app,
            );
            let _ = state.end_line.paint(
                end_origin,
                axis_font_size,
                TextAlign::Left,
                None,
                window,
                app,
            );
        },
    );
}

/// One wallet's timeline row: the axis, its markers, and the way to read it again.
pub(super) fn render_row(
    entry: &TimelineEntry,
    args: TimelineRowArgs,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let timeline_width = finite_width(entry.width.max(MIN_CONTENT_WIDTH));
    let labels = axis_labels(cx);
    let (start_label_width, _) = entry
        .paint_cache
        .borrow_mut()
        .axis_label_widths(labels.clone(), window);
    let geometry = entry.snapshot.as_ref().map_or_else(
        || {
            Arc::new(TimelineGeometry::new(
                &[],
                entry.plot_width,
                start_label_width,
            ))
        },
        |_| {
            entry.paint_cache.borrow_mut().geometry(
                &entry.buckets,
                entry.plot_width,
                start_label_width,
            )
        },
    );

    let body = match entry.snapshot.as_ref() {
        None => centered_body(
            args.failure.clone().unwrap_or_else(|| args.loading.clone()),
            if args.failure.is_some() {
                theme.roles.status.danger.base.hsla
            } else {
                theme.roles.fg.muted.hsla
            },
        ),
        Some(snapshot) => {
            let canvas = timeline_canvas(geometry.clone(), labels, entry.paint_cache.clone());
            let tooltip_rows = entry.paint_cache.borrow_mut().tooltip_rows(
                snapshot,
                &entry.buckets,
                args.precision.as_ref(),
            );
            let markers = geometry
                .markers
                .iter()
                .filter_map(|marker| {
                    let rows = tooltip_rows
                        .get(&(marker.side, marker.bucket_index))
                        .cloned()?;
                    Some(render_marker(
                        args.precision.clone(),
                        &entry.tooltip_scrolls,
                        rows,
                        *marker,
                        cx,
                    ))
                })
                .collect::<Vec<_>>();
            div()
                .relative()
                .w(px(timeline_width))
                .h(px(BODY_HEIGHT))
                .child(canvas)
                .children(markers)
                .into_any_element()
        }
    };

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap_2()
        .h(px(HEADER_HEIGHT))
        .text_xs()
        .when_some(args.failure.clone(), |this, failure| {
            let color = theme.roles.status.danger.base.hsla;
            this.child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_color(color)
                    .child(failure),
            )
        })
        .when_some(args.retry, |this, retry| this.child(retry));

    let timeline = div()
        .id(ElementId::Name(SharedString::new_static(
            "HyperliquidWalletTimeline",
        )))
        .w(px(timeline_width + HORIZONTAL_PADDING))
        .flex_none()
        .h(px(HEIGHT))
        .px_2()
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(header)
        .child(body);

    let measure_cache = entry.paint_cache.clone();
    let on_measure = args.on_measure.clone();
    let measure_labels = axis_labels(cx);
    let measure = canvas(
        move |bounds, window, cx| {
            let viewport_width = bounds.size.width.as_f32();
            if !viewport_width.is_finite() || viewport_width <= 0.0 {
                return;
            }
            let width = (viewport_width - HORIZONTAL_PADDING).max(1.0);
            let plot_width =
                measure_cache
                    .borrow_mut()
                    .plot_width(width, measure_labels.clone(), window);
            on_measure(width, plot_width, cx);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0();

    let viewport = div()
        .id(ElementId::Name(SharedString::new_static(
            "HyperliquidWalletTimelineViewport",
        )))
        .absolute()
        .inset_0()
        .bottom(SCROLLBAR_THICKNESS)
        .min_w_0()
        .min_h_0()
        .overflow_x_scroll()
        .track_scroll(&entry.scroll)
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(measure)
        .child(timeline);

    div()
        .relative()
        .min_w_0()
        .flex_1()
        .child(viewport)
        .child(
            div()
                .id(ElementId::Name(SharedString::new_static(
                    "HyperliquidWalletTimelineScrollbar",
                )))
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(SCROLLBAR_THICKNESS)
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(Scrollbar::horizontal(entry.scroll.clone())),
        )
        .into_any_element()
}

/// The control that asks for one wallet's timeline again.
pub(super) fn retry_button(
    label: SharedString,
    color: Hsla,
    on_retry: impl Fn(&mut App) + 'static,
) -> AnyElement {
    Button::new(ElementId::Name(SharedString::new_static(
        "HyperliquidTimelineRetry",
    )))
    .ghost()
    .small()
    .aria_label(label.clone())
    .with_tooltip(label)
    .on_click(move |_, _, cx| {
        cx.stop_propagation();
        on_retry(cx);
    })
    .child(
        Icon::new(IconKind::ArrowsClockwise)
            .size_3()
            .text_color(color),
    )
    .into_any_element()
}

/// The two labels the axis is drawn between.
fn axis_labels(cx: &App) -> (SharedString, SharedString) {
    (
        t!(cx, "widget.hyperliquid_wallet.timeline_start"),
        t!(cx, "widget.hyperliquid_wallet.timeline_end"),
    )
}

fn centered_body(message: SharedString, color: Hsla) -> AnyElement {
    div()
        .h(px(BODY_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .px_4()
        .text_center()
        .text_xs()
        .text_color(color)
        .child(message)
        .into_any_element()
}

/// One bucket marker: a transparent hit area over the painted square, and the card that lists
/// what was filled in it.
fn render_marker(
    precision: Option<DisplayPrecision>,
    tooltip_scrolls: &AHashMap<(Side, usize), UniformListScrollHandle>,
    rows: Arc<[TooltipRow]>,
    marker: MarkerGeometry,
    cx: &App,
) -> AnyElement {
    let side_key = match marker.side {
        Side::Buy => 0_u64,
        Side::Sell => 1_u64,
    };
    // One timeline's markers are siblings, so what a marker's id has to carry is the bucket it
    // sits in and which side of the book it is: everything the card draws hangs under that id.
    let marker_id = (marker.bucket_index as u64).rotate_left(17) ^ side_key;
    let visible_rows = rows.len().clamp(1, TOOLTIP_VISIBLE_ROWS);
    let show_scrollbar = rows.len() > TOOLTIP_VISIBLE_ROWS;
    let height = px(TOOLTIP_ROW_HEIGHT * visible_rows as f32);
    let scroll = tooltip_scrolls
        .get(&(marker.side, marker.bucket_index))
        .cloned()
        .unwrap_or_default();
    let side = marker.side;
    // The count decides the sentence: a marker holding one fill is not "1 fills".
    let side_label = match side {
        Side::Buy => t!(cx, "widget.hyperliquid_wallet.buy"),
        Side::Sell => t!(cx, "widget.hyperliquid_wallet.sell"),
    };
    let label = if rows.len() == 1 {
        t!(
            cx,
            "widget.hyperliquid_wallet.timeline_marker_one",
            side = side_label,
            value = rows.len()
        )
    } else {
        t!(
            cx,
            "widget.hyperliquid_wallet.timeline_marker",
            side = side_label,
            value = rows.len()
        )
    };
    let trigger = div()
        .id(ElementId::Name(SharedString::new_static(
            "HyperliquidTimelineMarkerTrigger",
        )))
        .w(marker.bounds.size.width)
        .h(marker.bounds.size.height)
        .flex_none()
        .bg(transparent_black())
        .aria_label(label)
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_any_mouse_down(|_, window, _| window.prevent_default());

    // The wrapper keeps the trigger's hit area aligned with the marker the canvas painted: the
    // card's style belongs to the floating surface, not to the body's flex flow.
    div()
        .absolute()
        .w(marker.bounds.size.width)
        .h(marker.bounds.size.height)
        .left(marker.bounds.origin.x)
        .top(marker.bounds.origin.y)
        .child(
            HoverCard::new(ElementId::NamedInteger(
                SharedString::new_static("HyperliquidTimelineMarkerCard"),
                marker_id,
            ))
            .anchor(match side {
                Side::Buy => Anchor::TopCenter,
                Side::Sell => Anchor::BottomCenter,
            })
            .open_delay(HOVER_OPEN_DELAY)
            .close_delay(HOVER_CLOSE_DELAY)
            .trigger(trigger)
            .content({
                move |_, _, cx| {
                    let theme = cx.theme();
                    // The moment a row is read at is the moment the card was built, so a card
                    // that stays open does not renumber itself under the reader.
                    let now = now_millis().unwrap_or(0);
                    let list_rows = rows.clone();
                    let list = uniform_list(
                        ElementId::Name(SharedString::new_static("HyperliquidTimelineTooltipRows")),
                        list_rows.len(),
                        move |range, _, cx| {
                            range
                                .map(|index| {
                                    tooltip_row(
                                        &list_rows[index],
                                        side,
                                        precision.as_ref(),
                                        now,
                                        cx,
                                    )
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .h(height)
                    .w_full()
                    .track_scroll(&scroll);

                    div()
                        .id(ElementId::Name(SharedString::new_static(
                            "HyperliquidTimelineMarkerCardBody",
                        )))
                        .relative()
                        .w(px(TOOLTIP_WIDTH))
                        .h(height)
                        .overflow_hidden()
                        .rounded_sm()
                        .border_1()
                        .border_color(theme.roles.line.divider.hsla)
                        .bg(theme.roles.surface.overlay.hsla)
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right(if show_scrollbar {
                                    SCROLLBAR_THICKNESS
                                } else {
                                    px(0.0)
                                })
                                .bottom_0()
                                .overflow_hidden()
                                .child(list),
                        )
                        .when(show_scrollbar, |this| {
                            this.child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .right_0()
                                    .bottom_0()
                                    .w(SCROLLBAR_THICKNESS)
                                    .child(Scrollbar::vertical(scroll.clone())),
                            )
                        })
                }
            }),
        )
        .into_any_element()
}

/// One fill of a bucket, as the card draws it.
///
/// The time a row shows is the age it had when the card was built, and the moment it happened
/// is its accessible name: a row is read as "when", and the absolute moment is what that is
/// relative to.
fn tooltip_row(
    row: &TooltipRow,
    side: Side,
    precision: Option<&DisplayPrecision>,
    now: i64,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (quantity, price) = precision.map_or_else(
        || {
            (
                SharedString::new(row.quantity.value.to_string().as_str()),
                SharedString::new(row.price.value.to_string().as_str()),
            )
        },
        |precision| {
            (
                SharedString::new(row.quantity_text(precision, cx).as_str()),
                SharedString::new(row.price_text(precision, cx).as_str()),
            )
        },
    );
    let (market_label, market_color) = fill_label(row, side, cx);
    let quantity_text = format_smolstr!("{quantity} @ {price}");

    div()
        .h(px(TOOLTIP_ROW_HEIGHT))
        .w_full()
        .px_2()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .text_xs()
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .child(SharedString::new(quantity_text.as_str())),
        )
        .child(
            div()
                .flex_none()
                .text_color(market_color)
                .child(market_label),
        )
        .child(
            div()
                .id(ElementId::NamedInteger(
                    SharedString::new_static("HyperliquidTimelineTooltipRowTime"),
                    marker_row_id(row.timestamp),
                ))
                .flex_none()
                .aria_label(absolute_time(row.timestamp, data_time_zone(cx)))
                .text_color(theme.roles.fg.muted.hsla)
                .child(age_text(cx, now, row.timestamp)),
        )
        .into_any_element()
}

/// The age of a moment, as the one relative time every row of this widget is read by.
pub(super) fn age_text(cx: &App, now: i64, timestamp: i64) -> SharedString {
    match age_bucket(now, timestamp) {
        AgeBucket::Seconds(value) => {
            t!(cx, "widget.hyperliquid_wallet.age_seconds", value = value)
        }
        AgeBucket::Minutes(value) => {
            t!(cx, "widget.hyperliquid_wallet.age_minutes", value = value)
        }
        AgeBucket::Hours(value) => {
            t!(cx, "widget.hyperliquid_wallet.age_hours", value = value)
        }
        AgeBucket::Days(value) => {
            t!(cx, "widget.hyperliquid_wallet.age_days", value = value)
        }
    }
}

/// How many buckets a plot of `width` is divided into.
pub(super) fn bucket_count(width: f32) -> usize {
    if !width.is_finite() || width <= 0.0 {
        return 1;
    }
    (width / BUCKET_WIDTH).floor().max(1.0) as usize
}

/// How long one bucket covers.
pub(super) fn bucket_width_millis(width: f32) -> i64 {
    // A sub-millisecond bucket could not represent another timestamp boundary.
    let count = bucket_count(width).min(WINDOW_MILLIS as usize) as i64;
    (WINDOW_MILLIS + count - 1) / count
}

/// The width of the axis itself, once both labels have their room.
pub(super) fn axis_line_width(
    total_width: f32,
    start_label_width: f32,
    end_label_width: f32,
) -> f32 {
    if !total_width.is_finite() || total_width <= 0.0 {
        return 1.0;
    }
    let occupied = start_label_width.max(0.0) + end_label_width.max(0.0) + AXIS_LABEL_GAP * 2.0;
    (total_width - occupied).max(1.0)
}

/// Assigns every fill of a snapshot to the slot of the side it was traded on.
pub(super) fn build_buckets(snapshot: &TimelineSnapshot, width: f32) -> Vec<TimelineBucket> {
    let count = bucket_count(width);
    let bucket_width = bucket_width_millis(width);
    let window = snapshot.window_end.saturating_sub(snapshot.window_start);
    if window <= 0 {
        return Vec::new();
    }
    let mut buckets: AHashMap<(Side, usize), Vec<usize>> =
        AHashMap::with_capacity(snapshot.fills.len().min(count.saturating_mul(2)));
    for (fill_index, fill) in snapshot.fills.iter().enumerate() {
        if fill.timestamp < snapshot.window_start || fill.timestamp > snapshot.window_end {
            continue;
        }
        let offset = fill.timestamp - snapshot.window_start;
        let index = (offset / bucket_width).min((count - 1) as i64) as usize;
        buckets
            .entry((fill.side, index))
            .or_default()
            .push(fill_index);
    }
    let mut result = buckets
        .into_iter()
        .map(|((side, index), fill_indices)| TimelineBucket {
            side,
            index,
            fill_indices,
        })
        .collect::<Vec<_>>();
    result.sort_unstable_by(|left, right| {
        left.index
            .cmp(&right.index)
            .then_with(|| side_rank(left.side).cmp(&side_rank(right.side)))
    });
    result
}

/// The fills of one bucket, newest first.
pub(super) fn tooltip_rows(
    snapshot: &TimelineSnapshot,
    bucket: &TimelineBucket,
    precision: Option<&DisplayPrecision>,
) -> Vec<TooltipRow> {
    let mut indices = bucket.fill_indices.clone();
    indices.sort_by(|left, right| {
        snapshot.fills[*right]
            .timestamp
            .cmp(&snapshot.fills[*left].timestamp)
            .then_with(|| snapshot.fills[*right].tid.cmp(&snapshot.fills[*left].tid))
            .then_with(|| snapshot.fills[*right].hash.cmp(&snapshot.fills[*left].hash))
            .then_with(|| {
                snapshot.fills[*right]
                    .listing
                    .cmp(&snapshot.fills[*left].listing)
            })
            .then_with(|| {
                side_rank(snapshot.fills[*right].side).cmp(&side_rank(snapshot.fills[*left].side))
            })
    });
    indices
        .into_iter()
        .map(|fill_index| {
            let fill = &snapshot.fills[fill_index];
            TooltipRow {
                timestamp: fill.timestamp,
                listing: fill.listing.clone(),
                kind: precision.map_or(InstrumentKind::Perpetual, |precision| {
                    precision.listing_kind(&fill.listing)
                }),
                quantity: fill.quantity.clone(),
                price: fill.price.clone(),
                direction: perpetual_direction(fill),
            }
        })
        .collect()
}

/// The label and colour of one fill's card row.
///
/// A fill is read by what it did to a position, so a perpetual fill is named by the move it
/// made; a spot fill names the market it happened on.
fn fill_label(row: &TooltipRow, side: Side, cx: &App) -> (SharedString, gpui_kit::Hsla) {
    let theme = cx.theme();
    match row.kind {
        InstrumentKind::Spot => (
            t!(cx, "widget.hyperliquid_wallet.spot"),
            theme.roles.fg.default.hsla,
        ),
        InstrumentKind::Perpetual => match (side, row.direction) {
            (_, FillDirection::LongToShort) => (
                t!(cx, "widget.hyperliquid_wallet.long_to_short"),
                theme.roles.market.down.hsla,
            ),
            (_, FillDirection::ShortToLong) => (
                t!(cx, "widget.hyperliquid_wallet.short_to_long"),
                theme.roles.market.up.hsla,
            ),
            (_, FillDirection::Liquidated) => (
                t!(cx, "widget.hyperliquid_wallet.liquidated"),
                theme.roles.market.down.hsla,
            ),
            (_, FillDirection::AutoDeleveraged) => (
                t!(cx, "widget.hyperliquid_wallet.auto_deleveraged"),
                theme.roles.fg.default.hsla,
            ),
            (Side::Buy, FillDirection::Long) => (
                t!(cx, "widget.hyperliquid_wallet.open_long"),
                theme.roles.market.up.hsla,
            ),
            (Side::Buy, FillDirection::Short) => (
                t!(cx, "widget.hyperliquid_wallet.close_short"),
                theme.roles.market.down.hsla,
            ),
            (Side::Sell, FillDirection::Long) => (
                t!(cx, "widget.hyperliquid_wallet.close_long"),
                theme.roles.market.up.hsla,
            ),
            (Side::Sell, FillDirection::Short) => (
                t!(cx, "widget.hyperliquid_wallet.open_short"),
                theme.roles.market.down.hsla,
            ),
            (_, FillDirection::Unknown) => (
                t!(cx, "widget.hyperliquid_wallet.unknown"),
                theme.roles.fg.default.hsla,
            ),
        },
        InstrumentKind::Future | InstrumentKind::Option | InstrumentKind::Outcome => (
            t!(cx, "widget.hyperliquid_wallet.unknown"),
            theme.roles.fg.default.hsla,
        ),
    }
}

/// Whether a fill opened or closed a long or a short position.
///
/// The venue's own answer is used when it gave one, and a fill that only says where the
/// position started is read from that: a settlement, or a fill with no direction at all, is
/// still on the side the position was on when it happened.
pub(super) fn perpetual_direction(fill: &HyperliquidUserFill) -> FillDirection {
    match fill.direction.as_ref() {
        Some(HyperliquidFillDirection::OpenLong | HyperliquidFillDirection::CloseLong) => {
            FillDirection::Long
        }
        Some(HyperliquidFillDirection::OpenShort | HyperliquidFillDirection::CloseShort) => {
            FillDirection::Short
        }
        Some(HyperliquidFillDirection::LongToShort) => FillDirection::LongToShort,
        Some(HyperliquidFillDirection::ShortToLong) => FillDirection::ShortToLong,
        Some(
            HyperliquidFillDirection::LiquidatedCrossLong
            | HyperliquidFillDirection::LiquidatedCrossShort
            | HyperliquidFillDirection::LiquidatedIsolatedLong
            | HyperliquidFillDirection::LiquidatedIsolatedShort
            | HyperliquidFillDirection::PartialBorrowLiquidation
            | HyperliquidFillDirection::BackstopBorrowLiquidation,
        ) => FillDirection::Liquidated,
        Some(HyperliquidFillDirection::AutoDeleveraging) => FillDirection::AutoDeleveraged,
        Some(
            HyperliquidFillDirection::Settlement
            | HyperliquidFillDirection::NetChildVaults
            | HyperliquidFillDirection::Buy
            | HyperliquidFillDirection::Sell
            | HyperliquidFillDirection::SpotDustConversion,
        )
        | None => {
            if fill.start_position > SignedQuantity::ZERO {
                FillDirection::Long
            } else if fill.start_position < SignedQuantity::ZERO {
                FillDirection::Short
            } else {
                FillDirection::Unknown
            }
        }
    }
}

fn side_rank(side: Side) -> u8 {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

/// A stable element id for a moment, for the row that shows it.
fn marker_row_id(timestamp: i64) -> u64 {
    (timestamp as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn axis_label_width(
    label: &SharedString,
    font_size: Pixels,
    text_style: &TextStyle,
    window: &Window,
) -> f32 {
    window
        .text_system()
        .shape_line(
            label.clone(),
            font_size,
            &[TextRun {
                len: label.len(),
                font: text_style.font(),
                ..Default::default()
            }],
            None,
        )
        .width()
        .as_f32()
}

#[cfg(test)]
mod tests {
    use market_participants_model::market::{account::AssetAmount, units::QuantityUnit};

    use super::*;
    use crate::test_support::{listing_key, market_price, perpetual_route, token};

    /// The listing every test fill was traded on.
    fn listing() -> VenueListingKey {
        listing_key(&perpetual_route(), "BTC")
    }

    fn fill(timestamp: i64, tid: u64, side: Side) -> HyperliquidUserFill {
        HyperliquidUserFill {
            address: "0x1111111111111111111111111111111111111111"
                .parse()
                .expect("address should be valid"),
            listing: listing(),
            side,
            quantity: MarketQuantity::traded(
                "1".parse().expect("quantity should be valid"),
                QuantityUnit::Contracts,
            )
            .expect("a fill quantity is a traded quantity"),
            price: market_price("1"),
            timestamp,
            start_position: SignedQuantity::ZERO,
            closed_pnl: AssetAmount {
                asset: token("USDC"),
                value: SignedQuantity::ZERO,
            },
            fee: AssetAmount {
                asset: token("USDC"),
                value: SignedQuantity::ZERO,
            },
            hash: None,
            tid,
            liquidation: false,
            direction: None,
        }
    }

    fn snapshot(fills: Vec<HyperliquidUserFill>) -> TimelineSnapshot {
        TimelineSnapshot {
            window_start: 0,
            window_end: WINDOW_MILLIS,
            fills: fills.into(),
        }
    }

    #[test]
    fn each_fill_is_assigned_to_one_side_bucket() {
        let snapshot = snapshot(vec![
            fill(1, 1, Side::Buy),
            fill(WINDOW_MILLIS, 2, Side::Sell),
        ]);

        let assigned = build_buckets(&snapshot, 560.0)
            .iter()
            .map(|bucket| bucket.fill_indices.len())
            .sum::<usize>();

        assert_eq!(assigned, 2);
    }

    #[test]
    fn fills_outside_the_window_are_left_out() {
        let snapshot = TimelineSnapshot {
            window_start: 100,
            window_end: 200,
            fills: vec![fill(99, 1, Side::Buy), fill(201, 2, Side::Sell)].into(),
        };

        assert!(build_buckets(&snapshot, 560.0).is_empty());
    }

    #[test]
    fn tooltip_rows_are_newest_first() {
        let snapshot = snapshot(vec![fill(1, 1, Side::Buy), fill(2, 2, Side::Buy)]);
        let bucket = build_buckets(&snapshot, 560.0)
            .into_iter()
            .find(|bucket| bucket.side == Side::Buy)
            .expect("a buy bucket should exist");

        let rows = tooltip_rows(&snapshot, &bucket, None);

        assert_eq!(rows[0].timestamp, 2);
    }

    #[test]
    fn bucket_count_and_width_follow_plot_width() {
        assert_eq!(bucket_count(23.0), 1);
        assert_eq!(bucket_count(560.0), 23);
        assert_eq!(bucket_count(576.0), 24);
        assert_eq!(bucket_width_millis(560.0), (WINDOW_MILLIS + 22) / 23);
        assert_eq!(bucket_width_millis(f32::MAX), 1);
    }

    #[test]
    fn axis_line_width_reserves_both_endpoint_labels() {
        let line_width = axis_line_width(560.0, 48.0, 24.0);

        assert_eq!(line_width, 472.0);
        assert_eq!(bucket_count(line_width), 19);
    }

    #[test]
    fn direction_prefers_api_value_and_falls_back_to_position() {
        let mut fill = fill(1, 1, Side::Buy);

        fill.direction = Some(HyperliquidFillDirection::OpenShort);
        assert_eq!(perpetual_direction(&fill), FillDirection::Short);

        fill.direction = Some(HyperliquidFillDirection::LongToShort);
        assert_eq!(perpetual_direction(&fill), FillDirection::LongToShort);

        fill.direction = Some(HyperliquidFillDirection::LiquidatedCrossLong);
        assert_eq!(perpetual_direction(&fill), FillDirection::Liquidated);

        fill.direction = Some(HyperliquidFillDirection::AutoDeleveraging);
        assert_eq!(perpetual_direction(&fill), FillDirection::AutoDeleveraged);

        fill.direction = Some(HyperliquidFillDirection::Settlement);
        fill.start_position = "2".parse().expect("position should be valid");
        assert_eq!(perpetual_direction(&fill), FillDirection::Long);

        fill.direction = Some(HyperliquidFillDirection::Settlement);
        fill.start_position = "-2".parse().expect("position should be valid");
        assert_eq!(perpetual_direction(&fill), FillDirection::Short);

        fill.direction = None;
        fill.start_position = SignedQuantity::ZERO;
        assert_eq!(perpetual_direction(&fill), FillDirection::Unknown);
    }

    #[test]
    fn tooltip_rows_keep_every_fill_of_a_bucket() {
        let snapshot = snapshot(
            (0..8)
                .map(|index| fill(index, index as u64, Side::Buy))
                .collect::<Vec<_>>(),
        );
        let bucket = build_buckets(&snapshot, 560.0)
            .into_iter()
            .find(|bucket| bucket.side == Side::Buy)
            .expect("a buy bucket should exist");

        assert_eq!(tooltip_rows(&snapshot, &bucket, None).len(), 8);
        assert!(tooltip_rows(&snapshot, &bucket, None).len() > TOOLTIP_VISIBLE_ROWS);
    }

    #[test]
    fn marker_bounds_are_square_and_have_a_visible_gap() {
        let buckets = vec![
            TimelineBucket {
                side: Side::Buy,
                index: 2,
                fill_indices: vec![0],
            },
            TimelineBucket {
                side: Side::Buy,
                index: 3,
                fill_indices: vec![1],
            },
            TimelineBucket {
                side: Side::Sell,
                index: 2,
                fill_indices: vec![2],
            },
        ];
        let geometry = TimelineGeometry::new(&buckets, 472.0, 48.0);
        assert!(
            geometry
                .markers
                .iter()
                .all(|marker| marker.bounds.size.width == marker.bounds.size.height)
        );
        let buys = geometry
            .markers
            .iter()
            .filter(|marker| marker.side == Side::Buy)
            .collect::<Vec<_>>();
        assert!(buys[1].bounds.left() - buys[0].bounds.right() >= px(MARKER_GAP - 0.01));
        assert!(
            geometry
                .markers
                .iter()
                .filter(|marker| marker.side == Side::Buy)
                .all(|marker| marker.connector.size.height > px(0.0))
        );
    }

    #[test]
    fn marker_gap_is_preserved_across_supported_plot_widths() {
        for plot_width in [472.0, 560.0, 768.0, 1_024.0, 1_536.0, 2_048.0] {
            let count = bucket_count(plot_width);
            let buckets = (0..count)
                .map(|index| TimelineBucket {
                    side: Side::Buy,
                    index,
                    fill_indices: vec![index],
                })
                .collect::<Vec<_>>();
            let geometry = TimelineGeometry::new(&buckets, plot_width, 48.0);
            for pair in geometry.markers.windows(2) {
                assert!(
                    pair[1].bounds.left() - pair[0].bounds.right() >= px(MARKER_GAP - 0.01),
                    "plot width {plot_width} produced an overlapping marker pair"
                );
            }
        }
    }

    #[test]
    fn a_bucket_beyond_the_plot_is_not_drawn() {
        let buckets = vec![TimelineBucket {
            side: Side::Buy,
            index: usize::MAX,
            fill_indices: vec![0],
        }];

        assert!(
            TimelineGeometry::new(&buckets, 560.0, 48.0)
                .markers
                .is_empty()
        );
    }

    #[test]
    fn opposite_sides_share_an_x_slot_without_overlapping() {
        let buckets = vec![
            TimelineBucket {
                side: Side::Buy,
                index: 4,
                fill_indices: vec![0],
            },
            TimelineBucket {
                side: Side::Sell,
                index: 4,
                fill_indices: vec![1],
            },
        ];
        let geometry = TimelineGeometry::new(&buckets, 560.0, 48.0);
        let [buy, sell] = geometry.markers.as_ref() else {
            panic!("expected one buy and one sell marker");
        };

        assert_eq!(buy.bounds.left(), sell.bounds.left());
        assert!(buy.bounds.bottom() < sell.bounds.top());
    }

    #[test]
    fn narrow_slots_use_available_space_to_keep_markers_separate() {
        let size = marker_size(20.0);

        assert_eq!(size, 14.0);
        assert!(20.0 - size >= MARKER_GAP);
    }

    #[test]
    fn geometry_cache_reuses_the_same_snapshot_and_size() {
        let snapshot = snapshot(vec![fill(1, 1, Side::Buy)]);
        let buckets: Arc<[TimelineBucket]> = build_buckets(&snapshot, 472.0).into();
        let mut cache = TimelinePaintCache::default();

        let first = cache.geometry(&buckets, 472.0, 48.0);
        let second = cache.geometry(&buckets, 472.0, 48.0);

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn geometry_cache_rebuilds_when_the_plot_width_changes() {
        let snapshot = snapshot(vec![fill(1, 1, Side::Buy)]);
        let buckets: Arc<[TimelineBucket]> = build_buckets(&snapshot, 472.0).into();
        let mut cache = TimelinePaintCache::default();

        let first = cache.geometry(&buckets, 472.0, 48.0);
        let second = cache.geometry(&buckets, 576.0, 48.0);

        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn entry_uses_arc_identity_for_snapshot_noop_detection() {
        let snapshot = Arc::new(snapshot(vec![fill(1, 1, Side::Buy)]));
        let mut entry = TimelineEntry::default();

        assert!(entry.replace_snapshot(&snapshot));
        assert!(!entry.replace_snapshot(&snapshot.clone()));
        assert!(!entry.replace_snapshot(&snapshot));
    }

    #[test]
    fn entry_dimension_changes_rebucket_and_retain_scroll_handles() {
        let snapshot = Arc::new(snapshot(vec![fill(1, 1, Side::Buy)]));
        let mut entry = TimelineEntry::default();
        entry.replace_snapshot(&snapshot);
        let handle = entry
            .tooltip_scrolls
            .get(&(Side::Buy, 0))
            .cloned()
            .expect("the first bucket should have a scroll handle");

        assert!(entry.set_dimensions(320.0, 320.0));
        assert!(
            entry
                .tooltip_scrolls
                .values()
                .any(|candidate| Rc::ptr_eq(&candidate.0, &handle.0))
        );
        assert!(!entry.set_dimensions(320.0, 320.0));
    }

    #[test]
    fn dimension_changes_rebucket_when_a_small_delta_crosses_a_bucket_boundary() {
        let snapshot = Arc::new(snapshot(vec![fill(1, 1, Side::Buy)]));
        let mut entry = TimelineEntry::default();
        entry.replace_snapshot(&snapshot);

        assert!(entry.set_dimensions(600.0, 575.9));
        assert_eq!(bucket_count(entry.plot_width), 23);
        assert!(entry.set_dimensions(600.0, 576.1));
        assert_eq!(bucket_count(entry.plot_width), 24);
    }

    #[test]
    fn a_non_finite_dimension_should_not_poison_the_plot_width() {
        let mut entry = TimelineEntry::default();

        assert!(entry.set_dimensions(f32::NAN, f32::INFINITY));
        assert_eq!(entry.plot_width, 1.0);
        assert_eq!(bucket_count(entry.plot_width), 1);
    }
}
