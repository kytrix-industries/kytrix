//! One wallet's last six hours of fills, drawn as an axis with a marker per bucket.
//!
//! # Why it is painted rather than laid out
//!
//! A bucket can hold dozens of fills and the axis is rebuilt whenever the window changes, so
//! the markers are painted by one canvas instead of being built as elements: a hundred
//! absolutely positioned divs would be a hundred elements the layout engine has to place on
//! every frame, and none of them would be cheaper to draw than a rectangle.
//!
//! The canvas is a **value**: the node is built once by the row factory and every frame that
//! has something new to draw hands it a [`Canvas`] through
//! [`WidgetCx::set_canvas`](widget::cx::WidgetCx::set_canvas). What a frame *keeps* is the
//! prepared key beside the canvas, so a steady `sync` — a snapshot that has not moved, at the
//! same box, in the same appearance, language and with the pointer where it was — writes no
//! canvas and asks for no frame (I3). Everything the drawing is a function of is in that key:
//! the snapshot's identity, the node's bounds, the axis geometry the labels give, the hovered
//! marker, the colours an appearance resolves, the language and the raster scale the labels
//! were shaped at.
//!
//! # What is cached, and against what
//!
//! The geometry of a timeline is a pure function of three things — the buckets, the plot width
//! and the width of the start label — so it is cached against exactly those three and thrown
//! away when one of them changes. Shaped text is cached the same way, against the text and its
//! style (which carries the colour and the size), which is what keeps a frame that only moves
//! the pointer from re-shaping the same labels. A handle whose atlas lease has gone is
//! re-shaped rather than drawn: the engine refuses to encode stale glyph runs, and the frame
//! that ran into one is rebuilt instead of kept.
//!
//! # The reader's fills are drawn, not laid out
//!
//! A marker's fills used to be a hover card on the toolkit's popup primitive with a scrollable
//! uniform list in it; both are gone. The pointer's position is passed in by the row, the
//! margin the marker occupies is hit-tested here, and the newest fills of that one bucket are
//! painted over the axis as a compact panel — up to [`TOOLTIP_VISIBLE_ROWS`] of them, fewer if
//! the band is too short to hold them. Two behaviours of the old card are therefore gone with
//! it, and they are the only behavioural differences of this port:
//!
//! - **no hover delays**: the open/close delays belonged to the popup primitive, so the panel
//!   is drawn for the marker under the pointer immediately. The panel still keeps itself open
//!   while the pointer is inside *it*, because two positions inside one panel are one drawing;
//! - **no scrollable list**: the panel draws the newest rows it has room for instead of a
//!   scroll handle per bucket. That is also why the old per-bucket `UniformListScrollHandle`
//!   map and its release are gone.
//!
//! The old timeline also scrolled horizontally below its minimum content width; the row now
//! gives the canvas the list's width and a narrow pane simply draws a narrower plot (the
//! bucket count follows the width, down to one bucket).
//!
//! # The axis labels are furniture
//!
//! The two ends of the axis are its time ticks, so they are read in the chart's own axis
//! colour, not in a text role: a chart's labels are its furniture. The letter inside a marker
//! is painted in the window's own background, which reads as a cut-out whatever colour the
//! marker is.

use std::sync::Arc;

use ahash::AHashMap;
use base_ui::{
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, resolved::Theme, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
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
use ui_core::{
    a11y::{A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    math::{Point, Rect},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{Border, ClipRect, Quad, QuadPaint},
        style::StylePatch,
    },
    text::{ShapedTextHandle, TextStyle},
};
use widget::cx::WidgetCx;

use super::model::{AgeBucket, DisplayPrecision, TimelineSnapshot, age_bucket, now_millis};

/// The height of one wallet's timeline band.
///
/// The row builds a box of this height under the account line and the canvas fills it; the
/// geometry below is centred in it, so the axis does not depend on where in the row it sits.
pub(super) const HEIGHT: f32 = 136.0;

/// The window a timeline covers, which is also the window one request asks for.
pub(super) const WINDOW_MILLIS: i64 = 6 * 60 * 60 * 1_000;

/// The width one bucket occupies, which decides how many of them there are.
const BUCKET_WIDTH: f32 = 24.0;
/// The gap between the axis and the label at either end of it.
const AXIS_LABEL_GAP: f32 = 8.0;
/// Where the axis sits inside the band: the middle of it.
const AXIS_Y: f32 = HEIGHT / 2.0;
const AXIS_LINE_HEIGHT: f32 = 1.0;
const AXIS_TICK_HEIGHT: f32 = 8.0;
const MARKER_MIN_SIZE: f32 = 18.0;
const MARKER_MAX_SIZE: f32 = 24.0;
const MARKER_GAP: f32 = 6.0;
const MARKER_AXIS_GAP: f32 = 4.0;
const MARKER_RADIUS_RATIO: f32 = 0.22;
const MARKER_FONT_RATIO: f32 = 0.68;
/// The smallest a letter inside a marker is drawn.
const MARKER_MIN_FONT_SIZE: f32 = 10.0;
const CONNECTOR_WIDTH: f32 = 1.0;
/// The widest the panel of a marker's fills is drawn.
const TOOLTIP_WIDTH: f32 = 320.0;
const TOOLTIP_ROW_HEIGHT: f32 = 18.0;
/// How many fills of a bucket the panel shows at most.
const TOOLTIP_VISIBLE_ROWS: usize = 6;
const TOOLTIP_PADDING: f32 = 4.0;
/// The gap between the panel and the marker it opens beside, and between it and the band.
const TOOLTIP_GAP: f32 = 4.0;
/// The panel's corner radius: the toolkit's `rounded_sm` at this application's rem.
const TOOLTIP_CORNER_RADIUS: f32 = 4.0;
/// How many shaped lines one timeline keeps.
///
/// The two axis labels, the two marker letters and three lines for each of the panel's rows:
/// twice the working set of one frame, so a language or appearance change does not evict the
/// lines the frame before was drawn from.
const TEXT_CACHE_CAPACITY: usize = 64;

/// The debug id of the canvas node one row's timeline is drawn on.
const TIMELINE_SELECTOR: &str = "TEST_HYPERLIQUID_WALLET_TIMELINE";

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

/// One fill of a bucket, as the panel shows it.
///
/// A row carries the units it was read with rather than text: what a quantity counts and what
/// its price is quoted in are properties of the listing, so a panel that is drawn while another
/// reading arrives still names its own units.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TooltipRow {
    timestamp: i64,
    listing: VenueListingKey,
    kind: InstrumentKind,
    quantity: MarketQuantity,
    price: MarketPrice,
    direction: FillDirection,
}

impl TooltipRow {
    /// The quantity of this row, named in the unit its listing publishes it in.
    fn quantity_text(&self, precision: &DisplayPrecision, translate: &Translate) -> SmolStr {
        let value = precision
            .format_listing_quantity(&self.listing, self.quantity.value)
            .unwrap_or_else(|_| format_smolstr!("{}", self.quantity.value));
        match precision.quantity_unit_label(&self.quantity.unit, translate) {
            Some(unit) => format_smolstr!("{value} {unit}"),
            None => value,
        }
    }

    /// The price of this row, in the quote asset its listing publishes it against.
    fn price_text(&self, precision: &DisplayPrecision, translate: &Translate) -> SmolStr {
        let value = precision
            .format_listing_price(&self.listing, self.price.value)
            .unwrap_or_else(|_| format_smolstr!("{}", self.price.value));
        format_smolstr!(
            "{value} {}",
            precision.asset_label(&self.price.quote_asset, translate)
        )
    }
}

/// One marker's place on the axis, and the connector that joins it to the axis.
///
/// Coordinates are the canvas' own: the origin is the top-left of the box the node was laid
/// out in, and painting translates the whole geometry by that box's origin.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MarkerGeometry {
    side: Side,
    bucket_index: usize,
    bounds: Rect,
    connector: Rect,
}

/// Where everything on one timeline is drawn.
#[derive(Debug, PartialEq)]
struct TimelineGeometry {
    axis: Rect,
    start_tick: Rect,
    end_tick: Rect,
    markers: Arc<[MarkerGeometry]>,
    marker_size: f32,
}

impl TimelineGeometry {
    fn new(buckets: &[TimelineBucket], plot_width: f32, start_label_width: f32) -> Self {
        let plot_width = finite_width(plot_width);
        let bucket_count = bucket_count(plot_width);
        let slot_width = plot_width / bucket_count as f32;
        let marker_size = marker_size(slot_width);
        let axis_left = start_label_width.max(0.0) + AXIS_LABEL_GAP;
        let axis = Rect::new(axis_left, AXIS_Y, plot_width, AXIS_LINE_HEIGHT);
        let start_tick = Rect::new(
            axis_left,
            AXIS_Y - AXIS_TICK_HEIGHT / 2.0,
            AXIS_LINE_HEIGHT,
            AXIS_TICK_HEIGHT,
        );
        let end_tick = Rect::new(
            axis_left + plot_width - AXIS_LINE_HEIGHT,
            AXIS_Y - AXIS_TICK_HEIGHT / 2.0,
            AXIS_LINE_HEIGHT,
            AXIS_TICK_HEIGHT,
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
                    let bottom = AXIS_Y - MARKER_AXIS_GAP;
                    (
                        bottom - marker_size,
                        bottom,
                        (AXIS_Y - bottom).max(AXIS_LINE_HEIGHT),
                    )
                }
                Side::Sell => {
                    let top = AXIS_Y + AXIS_LINE_HEIGHT + MARKER_AXIS_GAP;
                    (
                        top,
                        AXIS_Y + AXIS_LINE_HEIGHT,
                        (top - (AXIS_Y + AXIS_LINE_HEIGHT)).max(AXIS_LINE_HEIGHT),
                    )
                }
            };
            let bounds = Rect::new(left, top, marker_size, marker_size);
            let connector = Rect::new(
                center_x - CONNECTOR_WIDTH / 2.0,
                connector_top.min(top + marker_size),
                CONNECTOR_WIDTH,
                connector_height,
            );
            markers.push(MarkerGeometry {
                side: bucket.side,
                bucket_index: bucket.index,
                bounds,
                connector,
            });
        }

        Self {
            axis,
            start_tick,
            end_tick,
            markers: markers.into(),
            marker_size,
        }
    }

    /// The marker of one bucket side, once the geometry was built.
    fn marker(&self, side: Side, bucket_index: usize) -> Option<&MarkerGeometry> {
        self.markers
            .iter()
            .find(|marker| marker.side == side && marker.bucket_index == bucket_index)
    }

    /// The marker under `point`, if any.
    ///
    /// The markers of one bucket are painted in order, so the last one that contains the
    /// point is the one a reader is pointing at.
    fn marker_at(&self, point: Point) -> Option<&MarkerGeometry> {
        self.markers
            .iter()
            .rev()
            .find(|marker| marker.bounds.contains(point))
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

/// The width the plot is drawn at inside a canvas `width` wide.
///
/// The canvas *is* the plot: the row lays it out across the list's width and the two end
/// labels are shaped into the margins of the axis in [`TimelinePaint::sync`]. The guard is the
/// one old entry point had — a layout that has not run yet answers `0`, and a non-finite box
/// must not reach the bucket math — so a canvas that is not laid out yet draws nothing rather
/// than one degenerate plot.
pub(super) fn plot_width(width: f32) -> f32 {
    finite_width(width)
}

/// The width of the axis itself, once both labels have their room.
fn axis_line_width(total_width: f32, start_label_width: f32, end_label_width: f32) -> f32 {
    if !total_width.is_finite() || total_width <= 0.0 {
        return 1.0;
    }
    let occupied = start_label_width.max(0.0) + end_label_width.max(0.0) + AXIS_LABEL_GAP * 2.0;
    (total_width - occupied).max(1.0)
}

/// How many buckets a plot of `width` is divided into.
fn bucket_count(width: f32) -> usize {
    if !width.is_finite() || width <= 0.0 {
        return 1;
    }
    (width / BUCKET_WIDTH).floor().max(1.0) as usize
}

/// How long one bucket covers.
fn bucket_width_millis(width: f32) -> i64 {
    // A sub-millisecond bucket could not represent another timestamp boundary.
    let count = bucket_count(width).min(WINDOW_MILLIS as usize) as i64;
    (WINDOW_MILLIS + count - 1) / count
}

/// Assigns every fill of a snapshot to the slot of the side it was traded on.
fn build_buckets(snapshot: &TimelineSnapshot, width: f32) -> Vec<TimelineBucket> {
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
fn tooltip_rows(
    snapshot: &TimelineSnapshot,
    bucket: &TimelineBucket,
    precision: &DisplayPrecision,
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
                kind: precision.listing_kind(&fill.listing),
                quantity: fill.quantity.clone(),
                price: fill.price.clone(),
                direction: perpetual_direction(fill),
            }
        })
        .collect()
}

/// Whether a fill opened or closed a long or a short position.
///
/// The venue's own answer is used when it gave one, and a fill that only says where the
/// position started is read from that: a settlement, or a fill with no direction at all, is
/// still on the side the position was on when it happened.
fn perpetual_direction(fill: &HyperliquidUserFill) -> FillDirection {
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

/// What a fill's panel row says about the position it moved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FillTone {
    Up,
    Down,
    Neutral,
}

/// The label and tone of one fill's panel row.
///
/// A fill is read by what it did to a position, so a perpetual fill is named by the move it
/// made; a spot fill names the market it happened on.
fn fill_label(row: &TooltipRow, side: Side, translate: &Translate) -> (SmolStr, FillTone) {
    match row.kind {
        InstrumentKind::Spot => (
            t_smol!(translate, "widget.hyperliquid_wallet.spot"),
            FillTone::Neutral,
        ),
        InstrumentKind::Perpetual => match (side, row.direction) {
            (_, FillDirection::LongToShort) => (
                t_smol!(translate, "widget.hyperliquid_wallet.long_to_short"),
                FillTone::Down,
            ),
            (_, FillDirection::ShortToLong) => (
                t_smol!(translate, "widget.hyperliquid_wallet.short_to_long"),
                FillTone::Up,
            ),
            (_, FillDirection::Liquidated) => (
                t_smol!(translate, "widget.hyperliquid_wallet.liquidated"),
                FillTone::Down,
            ),
            (_, FillDirection::AutoDeleveraged) => (
                t_smol!(translate, "widget.hyperliquid_wallet.auto_deleveraged"),
                FillTone::Neutral,
            ),
            (Side::Buy, FillDirection::Long) => (
                t_smol!(translate, "widget.hyperliquid_wallet.open_long"),
                FillTone::Up,
            ),
            (Side::Buy, FillDirection::Short) => (
                t_smol!(translate, "widget.hyperliquid_wallet.close_short"),
                FillTone::Down,
            ),
            (Side::Sell, FillDirection::Long) => (
                t_smol!(translate, "widget.hyperliquid_wallet.close_long"),
                FillTone::Up,
            ),
            (Side::Sell, FillDirection::Short) => (
                t_smol!(translate, "widget.hyperliquid_wallet.open_short"),
                FillTone::Down,
            ),
            (_, FillDirection::Unknown) => (
                t_smol!(translate, "widget.hyperliquid_wallet.unknown"),
                FillTone::Neutral,
            ),
        },
        InstrumentKind::Future | InstrumentKind::Option | InstrumentKind::Outcome => (
            t_smol!(translate, "widget.hyperliquid_wallet.unknown"),
            FillTone::Neutral,
        ),
    }
}

/// The age of a moment, as the one relative time every row of this widget is read by.
///
/// The detail tables are the widget's other reader of it: a table's time cell and a timeline's
/// tooltip are the same sentence about the same clock, so they are one function.
pub(super) fn age_text(now: i64, timestamp: i64, translate: &Translate) -> SmolStr {
    match age_bucket(now, timestamp) {
        AgeBucket::Seconds(value) => {
            t_smol!(
                translate,
                "widget.hyperliquid_wallet.age_seconds",
                value = value
            )
        }
        AgeBucket::Minutes(value) => {
            t_smol!(
                translate,
                "widget.hyperliquid_wallet.age_minutes",
                value = value
            )
        }
        AgeBucket::Hours(value) => {
            t_smol!(
                translate,
                "widget.hyperliquid_wallet.age_hours",
                value = value
            )
        }
        AgeBucket::Days(value) => {
            t_smol!(
                translate,
                "widget.hyperliquid_wallet.age_days",
                value = value
            )
        }
    }
}

/// The two labels the axis is drawn between.
fn axis_labels(translate: &Translate) -> (SmolStr, SmolStr) {
    (
        t_smol!(translate, "widget.hyperliquid_wallet.timeline_start"),
        t_smol!(translate, "widget.hyperliquid_wallet.timeline_end"),
    )
}

/// The colours one frame of a timeline is drawn with, resolved from the theme.
///
/// The canvas bakes them in, which is why the appearance is one of the prepared key's inputs:
/// a canvas built for the light appearance is the wrong value for the dark one.
#[derive(Clone, Copy, Debug, PartialEq)]
struct TimelineColors {
    /// The axis line, its ticks and the connectors: chart grid, not a text colour.
    axis: Color,
    /// The two end labels: chart furniture, read in the axis colour.
    axis_label: Color,
    /// Marker outlines and the panel's edge.
    divider: Color,
    up: Color,
    down: Color,
    /// The letter inside a marker, painted in the surface it cuts out of.
    marker_text: Color,
    neutral: Color,
    muted: Color,
    /// The panel a marker's fills are drawn on, which sits over everything else.
    panel: Color,
}

impl TimelineColors {
    fn resolve(theme: &Theme) -> Self {
        Self {
            axis: theme.role(Role::ChartGrid),
            axis_label: theme.role(Role::ChartAxis),
            divider: theme.role(Role::LineDivider),
            up: theme.role(Role::MarketUp),
            down: theme.role(Role::MarketDown),
            marker_text: theme.role(Role::SurfaceSunken),
            neutral: theme.role(Role::FgDefault),
            muted: theme.role(Role::FgMuted),
            panel: theme.role(Role::SurfaceOverlay),
        }
    }

    fn tone(self, tone: FillTone) -> Color {
        match tone {
            FillTone::Up => self.up,
            FillTone::Down => self.down,
            FillTone::Neutral => self.neutral,
        }
    }
}

fn text_style(font_size: f32, color: Color) -> TextStyle {
    let mut style = TextStyle::new(font_size);
    style.color = color;
    style
}

fn solid_quad(bounds: Rect, fill: Color) -> Quad {
    Quad {
        bounds,
        corner_radius: [0.0; 4],
        paint: QuadPaint::Solid { fill, border: None },
    }
}

/// What a timeline is showing, and the buckets its fills were assigned to.
struct TimelineData {
    snapshot: Option<Arc<TimelineSnapshot>>,
    buckets: Arc<[TimelineBucket]>,
    /// The plot width the buckets were assigned for.
    plot_width: f32,
}

impl Default for TimelineData {
    fn default() -> Self {
        Self {
            snapshot: None,
            buckets: Arc::from([]),
            plot_width: 1.0,
        }
    }
}

impl TimelineData {
    /// Takes a published snapshot, and says whether anything a frame draws changed.
    ///
    /// The ten-second cadence publishes the same window whenever a wallet has not traded, so an
    /// unchanged snapshot is recognised by identity rather than by comparing fills.
    fn replace_snapshot(&mut self, snapshot: Option<&Arc<TimelineSnapshot>>) -> bool {
        let same = match (&self.snapshot, snapshot) {
            (Some(current), Some(next)) => Arc::ptr_eq(current, next),
            (None, None) => true,
            _ => false,
        };
        if same {
            return false;
        }
        self.snapshot = snapshot.cloned();
        self.buckets = match &self.snapshot {
            Some(snapshot) => build_buckets(snapshot, self.plot_width.max(1.0)).into(),
            None => Arc::from([]),
        };
        true
    }

    /// Records the plot width the timeline was laid out at, and rebuckets if it changed.
    ///
    /// A width that moved by less than half a pixel is the same width — layout rounding must
    /// not throw the buckets away — but a move that crosses a bucket boundary is a new plot
    /// even when the pixel width barely changed.
    fn set_plot_width(&mut self, plot_width: f32) -> bool {
        let plot_width = finite_width(plot_width);
        let rebucket = bucket_count(self.plot_width) != bucket_count(plot_width);
        if !rebucket && (self.plot_width - plot_width).abs() <= 0.5 {
            return false;
        }
        self.plot_width = plot_width;
        if rebucket && let Some(snapshot) = self.snapshot.clone() {
            self.buckets = build_buckets(&snapshot, plot_width).into();
        }
        true
    }

    fn bucket(&self, side: Side, index: usize) -> Option<&TimelineBucket> {
        self.buckets
            .iter()
            .find(|bucket| bucket.side == side && bucket.index == index)
    }

    fn tooltip_row_count(&self, side: Side, index: usize) -> usize {
        self.bucket(side, index)
            .map_or(0, |bucket| bucket.fill_indices.len())
    }
}

/// One shaped line, kept until the text or the style it was shaped for changes.
struct CachedTextLine {
    text: SmolStr,
    style: TextStyle,
    line: Arc<ShapedTextHandle>,
}

impl CachedTextLine {
    fn matches(&self, text: &str, style: &TextStyle) -> bool {
        self.text == text && self.style == *style
    }
}

/// The geometry one prepared frame was laid out from.
struct GeometryCacheEntry {
    buckets: Arc<[TimelineBucket]>,
    plot_width: f32,
    start_label_width: f32,
    geometry: Arc<TimelineGeometry>,
}

/// Everything one timeline remembers between frames.
///
/// It is bounded: one geometry and a fixed number of shaped lines.
#[derive(Default)]
struct PaintCache {
    geometry: Option<GeometryCacheEntry>,
    text_lines: Vec<CachedTextLine>,
}

impl PaintCache {
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

    /// A shaped line for `text`, from the cache when it is already there.
    ///
    /// A handle whose atlas lease has gone is dropped and shaped again: the glyphs it named
    /// have been reused, so drawing it would be drawing another line's pixels.
    fn line(&mut self, app: &mut App, text: &str, style: &TextStyle) -> Arc<ShapedTextHandle> {
        if let Some(index) = self
            .text_lines
            .iter()
            .position(|entry| entry.matches(text, style))
        {
            // Refreshing recency by moving the entry to the back is what makes the capacity
            // below evict the least recently used line.
            let entry = self.text_lines.remove(index);
            if !entry.line.is_stale() {
                let line = entry.line.clone();
                self.text_lines.push(entry);
                return line;
            }
        }

        let line = Arc::new(app.shape_text(text, style, None, &[]).unwrap_or_else(|_| {
            // Text that cannot be shaped at all is drawn as nothing rather than failing the
            // frame: a label is not worth a dropped frame, and the next frame tries again.
            app.shape_text("", style, None, &[])
                .expect("an empty line always shapes")
        }));
        if self.text_lines.len() >= TEXT_CACHE_CAPACITY {
            self.text_lines.remove(0);
        }
        self.text_lines.push(CachedTextLine {
            text: SmolStr::new(text),
            style: style.clone(),
            line: line.clone(),
        });
        line
    }
}

/// The width of the two axis labels, and the lines they were shaped with.
struct LabelMetrics {
    labels: (SmolStr, SmolStr),
    color: Color,
    font_size: f32,
    text_scale: f32,
    start: Arc<ShapedTextHandle>,
    end: Arc<ShapedTextHandle>,
}

impl LabelMetrics {
    fn matches(
        &self,
        labels: &(SmolStr, SmolStr),
        color: Color,
        font_size: f32,
        text_scale: f32,
    ) -> bool {
        self.labels == *labels
            && self.color == color
            && self.font_size == font_size
            && self.text_scale == text_scale
    }
}

/// The inputs one canvas was built from.
///
/// Everything a frame reads has to be in here, or a change to it would keep serving the canvas
/// from before the change: the snapshot's identity, the box the node was laid out in, the two
/// label widths the axis geometry was a function of, the hovered marker, the colours the
/// appearance resolves, the language and the raster scale the labels were shaped at.
struct PreparedKey {
    snapshot: Option<Arc<TimelineSnapshot>>,
    bounds: Rect,
    axis_width: f32,
    start_label_width: f32,
    hovered: Option<(Side, usize)>,
    colors: TimelineColors,
    language: LanguageCode,
    text_scale: f32,
}

impl PreparedKey {
    fn matches(
        &self,
        snapshot: &Option<Arc<TimelineSnapshot>>,
        bounds: Rect,
        axis_width: f32,
        start_label_width: f32,
        hovered: Option<(Side, usize)>,
        colors: TimelineColors,
        language: LanguageCode,
        text_scale: f32,
    ) -> bool {
        let same_snapshot = match (&self.snapshot, snapshot) {
            (Some(current), Some(next)) => Arc::ptr_eq(current, next),
            (None, None) => true,
            _ => false,
        };
        same_snapshot
            && self.bounds == bounds
            && self.axis_width == axis_width
            && self.start_label_width == start_label_width
            && self.hovered == hovered
            && self.colors == colors
            && self.language == language
            && self.text_scale == text_scale
    }
}

/// One wallet's timeline: the canvas node it draws into, what it last drew, and the caches a
/// frame reuses.
pub(super) struct TimelinePaint {
    node: Option<NodeId>,
    data: TimelineData,
    cache: PaintCache,
    labels: Option<LabelMetrics>,
    /// What the canvas on screen was built from.
    prepared: Option<PreparedKey>,
    /// The shaped lines the canvas on screen was built from.
    ///
    /// A canvas is a value and the timeline keeps the one it built, but the *glyphs* a canvas
    /// names are leased from a shared atlas: an atlas generation change or a per-tile eviction
    /// leaves the handles it was built from pointing at reused texture, and the engine drops
    /// such runs at encode time. The lines recorded here are the ones whose lease a steady
    /// frame has to check.
    painted_text: Vec<Arc<ShapedTextHandle>>,
    /// The marker the panel is open on, kept so the pointer can move from the marker into the
    /// panel without the panel closing underneath it.
    hovered: Option<(Side, usize)>,
    /// The box the canvas was last cleared for, while the row has no reading to draw.
    empty_bounds: Option<Rect>,
    /// The language the canvas' accessible name was last written in.
    a11y_language: Option<LanguageCode>,
}

impl TimelinePaint {
    pub(super) fn new(node: NodeId) -> Self {
        Self {
            node: Some(node),
            data: TimelineData::default(),
            cache: PaintCache::default(),
            labels: None,
            prepared: None,
            painted_text: Vec::new(),
            hovered: None,
            empty_bounds: None,
            a11y_language: None,
        }
    }

    /// Repaints only when the frame this one would be is not the one already drawn.
    ///
    /// `hover` is the pointer inside the canvas, in the canvas' own coordinates, when the row
    /// is hovered. A canvas whose box the layout has not given yet has no width to divide into
    /// buckets, so it is left alone and the next `sync` draws it.
    pub(super) fn sync(
        &mut self,
        cx: &mut WidgetCx<'_>,
        snapshot: Option<&Arc<TimelineSnapshot>>,
        precision: &DisplayPrecision,
        language: Signal<LanguageCode>,
        hover: Option<Point>,
    ) -> Result<(), EngineError> {
        let Some(node) = self.node else {
            return Ok(());
        };
        let Ok(target) = cx.tree().get(node) else {
            return Ok(());
        };
        let bounds = target.layout().bounds;
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 || !bounds.is_finite() {
            // The frame that laid the node out has not run yet; the next one will.
            return Ok(());
        }

        let Some(snapshot) = snapshot else {
            // The row's own overlay says why there is nothing to draw: an empty canvas is what
            // the node holds until a reading arrives, and a second frame for the same box is
            // not a change.
            if self.empty_bounds == Some(bounds) {
                return Ok(());
            }
            cx.set_canvas(node, Canvas::new(bounds))?;
            self.empty_bounds = Some(bounds);
            self.prepared = None;
            self.painted_text.clear();
            self.hovered = None;
            self.data.replace_snapshot(None);
            return Ok(());
        };
        self.empty_bounds = None;

        let appearance = cx.theme().current(cx.app());
        let colors = TimelineColors::resolve(cx.theme().theme_for(appearance));
        let tokens = *cx.theme().tokens();
        let text_scale = cx.app().text_scale();
        let language_now = *cx.read(&language);
        let translate = cx.translate().clone();

        let plot_width = plot_width(bounds.size.width);
        self.data.replace_snapshot(Some(snapshot));
        self.data.set_plot_width(plot_width);

        let (labels, label_widths) = self.ensure_labels(
            cx.app_mut(),
            axis_labels(&translate),
            tokens.small.font_size,
            colors.axis_label,
            text_scale,
        );
        // A canvas too narrow to keep both labels beside the axis draws the axis alone rather
        // than over painting the box it was given.
        let (start_label_width, end_label_width) =
            if label_widths.0 + label_widths.1 + AXIS_LABEL_GAP * 2.0 < plot_width {
                label_widths
            } else {
                (0.0, 0.0)
            };
        let axis_width = axis_line_width(plot_width, start_label_width, end_label_width);
        let geometry = self
            .cache
            .geometry(&self.data.buckets, axis_width, start_label_width);
        let hovered = self.resolve_hover(hover, &geometry, bounds);

        let prepared = self.prepared.as_ref().is_some_and(|prepared| {
            prepared.matches(
                &self.data.snapshot,
                bounds,
                axis_width,
                start_label_width,
                hovered,
                colors,
                language_now,
                text_scale,
            )
        }) && !self.painted_text.iter().any(|handle| handle.is_stale());
        if prepared {
            return Ok(());
        }

        // A canvas is an image to assistive technology: the title the row's account line says
        // in words is what its drawing is called.
        if self.a11y_language != Some(language_now) {
            cx.set_a11y_patch(
                node,
                A11yPatch {
                    role: Some(A11yRole::Image),
                    label: Some(t_smol!(
                        translate,
                        "widget.hyperliquid_wallet.timeline_title"
                    )),
                    ..A11yPatch::default()
                },
            )?;
            self.a11y_language = Some(language_now);
        }

        let marker_font = (geometry.marker_size * MARKER_FONT_RATIO).max(MARKER_MIN_FONT_SIZE);
        let frame = Frame {
            app: cx.app_mut(),
            bounds,
            colors,
            axis_font: tokens.small.font_size,
            marker_font,
            border_width: tokens.border_width,
            translate: &translate,
            precision,
            now: now_millis().unwrap_or(0),
            snapshot,
            buckets: &self.data.buckets,
            geometry: &geometry,
            labels: if start_label_width > 0.0 {
                labels
            } else {
                (None, None)
            },
            hovered,
        };
        let (canvas, whole, painted_text) = frame.paint(&mut self.cache);
        cx.set_canvas(node, canvas)?;
        if whole {
            self.prepared = Some(PreparedKey {
                snapshot: self.data.snapshot.clone(),
                bounds,
                axis_width,
                start_label_width,
                hovered,
                colors,
                language: language_now,
                text_scale,
            });
            self.painted_text = painted_text;
        } else {
            // An incomplete canvas is not what the timeline draws: leaving the key unrecorded
            // is what makes the next frame build it again (I3 still holds — a whole frame is
            // still a short circuit).
            self.prepared = None;
            self.painted_text.clear();
        }
        Ok(())
    }

    /// Drops everything the timeline remembered about the row it was drawn in.
    ///
    /// The row's list removes the canvas node itself; a timeline that registered geometry
    /// would hand it back here, and this one registers none (quads and glyph runs only).
    pub(super) fn release(&mut self, _cx: &mut WidgetCx<'_>) {
        self.node = None;
        self.data = TimelineData::default();
        self.cache = PaintCache::default();
        self.labels = None;
        self.prepared = None;
        self.painted_text.clear();
        self.hovered = None;
        self.empty_bounds = None;
        self.a11y_language = None;
    }

    /// Shapes the two end labels, or reuses the lines and widths a frame already has.
    fn ensure_labels(
        &mut self,
        app: &mut App,
        labels: (SmolStr, SmolStr),
        font_size: f32,
        color: Color,
        text_scale: f32,
    ) -> (
        (Option<Arc<ShapedTextHandle>>, Option<Arc<ShapedTextHandle>>),
        (f32, f32),
    ) {
        let current = self.labels.as_ref().is_some_and(|metrics| {
            metrics.matches(&labels, color, font_size, text_scale)
                && !metrics.start.is_stale()
                && !metrics.end.is_stale()
        });
        if !current {
            let style = text_style(font_size, color);
            let start = self.cache.line(app, labels.0.as_str(), &style);
            let end = self.cache.line(app, labels.1.as_str(), &style);
            self.labels = Some(LabelMetrics {
                labels,
                color,
                font_size,
                text_scale,
                start,
                end,
            });
        }
        let metrics = self.labels.as_ref().expect("the labels are ensured");
        let widths = (metrics.start.size().width, metrics.end.size().width);
        (
            (Some(metrics.start.clone()), Some(metrics.end.clone())),
            widths,
        )
    }

    /// Records where the pointer is, answering the marker whose panel is open.
    ///
    /// Moving inside one marker is not a movement, and neither is moving from the marker into
    /// the panel it opened: the marker's panel staying open is what lets a reader point at it.
    fn resolve_hover(
        &mut self,
        hover: Option<Point>,
        geometry: &TimelineGeometry,
        bounds: Rect,
    ) -> Option<(Side, usize)> {
        let Some(point) = hover.filter(|point| point.x.is_finite() && point.y.is_finite()) else {
            self.hovered = None;
            return None;
        };
        if let Some(marker) = geometry.marker_at(point) {
            let hovered = (marker.side, marker.bucket_index);
            self.hovered = Some(hovered);
            return Some(hovered);
        }
        let previous = self.hovered?;
        let rows = self
            .data
            .tooltip_row_count(previous.0, previous.1)
            .min(tooltip_row_limit(bounds.size.height));
        if rows == 0 {
            self.hovered = None;
            return None;
        }
        let Some(marker) = geometry.marker(previous.0, previous.1) else {
            self.hovered = None;
            return None;
        };
        let panel = tooltip_panel(marker, bounds.size.width, bounds.size.height, rows);
        if panel.contains(point) {
            Some(previous)
        } else {
            self.hovered = None;
            None
        }
    }
}

/// Builds the canvas node inside a list row.
///
/// There is no `App` in a row factory: the node is built empty (`Rect::default()`) and the
/// first [`TimelinePaint::sync`] after it was laid out is what fills it in. The node registers
/// a theme dependency so an appearance switch invalidates it and schedules the frame whose
/// `sync` resolves the new colours (G3); the language dependency is the row's own bound title
/// text, whose write schedules the same frame.
pub(super) fn build_canvas(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: ThemeHandle,
) -> Result<NodeId, EngineError> {
    NodeBuilder::canvas(Canvas::new(Rect::default()))
        .id(TIMELINE_SELECTOR)
        .style(
            StyleBuilder::new()
                .size_full()
                .min_width_0()
                .min_height_0()
                .build(),
        )
        .tab_stop(false)
        .bind_style(move |read| {
            let _ = theme.appearance(read);
            StylePatch::default()
        })
        .build(tree, parent)
}

fn tooltip_row_limit(height: f32) -> usize {
    if !height.is_finite() || height <= 0.0 {
        return 0;
    }
    let available = height - TOOLTIP_GAP * 2.0 - TOOLTIP_PADDING * 2.0;
    let rows = (available / TOOLTIP_ROW_HEIGHT).floor().max(0.0) as usize;
    rows.min(TOOLTIP_VISIBLE_ROWS)
}

/// Where the panel of one marker's fills is drawn, in the canvas' coordinates.
fn tooltip_panel(marker: &MarkerGeometry, width: f32, height: f32, rows: usize) -> Rect {
    let panel_width = TOOLTIP_WIDTH.min((width - TOOLTIP_GAP * 2.0).max(1.0));
    let panel_height = (rows as f32 * TOOLTIP_ROW_HEIGHT + TOOLTIP_PADDING * 2.0)
        .min((height - TOOLTIP_GAP * 2.0).max(1.0));
    let center = marker.bounds.origin.x + marker.bounds.size.width / 2.0;
    let left = (center - panel_width / 2.0).clamp(
        TOOLTIP_GAP,
        (width - panel_width - TOOLTIP_GAP).max(TOOLTIP_GAP),
    );
    let top = match marker.side {
        Side::Buy => marker.bounds.origin.y - TOOLTIP_GAP - panel_height,
        Side::Sell => marker.bounds.max().y + TOOLTIP_GAP,
    }
    .clamp(
        TOOLTIP_GAP,
        (height - panel_height - TOOLTIP_GAP).max(TOOLTIP_GAP),
    );
    Rect::new(left, top, panel_width, panel_height)
}

/// The shaped text one frame pushes, and whether every line reached the canvas.
struct TextPainter<'a> {
    app: &'a mut App,
    cache: &'a mut PaintCache,
    /// The lines this frame pushed, which are the canvas' text.
    used: Vec<Arc<ShapedTextHandle>>,
    whole: bool,
}

impl TextPainter<'_> {
    /// The shaped line of `text`, or `None` for text that has nothing to draw.
    fn line(&mut self, text: &str, style: &TextStyle) -> Option<Arc<ShapedTextHandle>> {
        if text.is_empty() {
            return None;
        }
        Some(self.cache.line(self.app, text, style))
    }

    /// Pushes one shaped line at `origin`.
    ///
    /// A refused line is one whose atlas lease died between being shaped and being pushed: its
    /// glyphs have been reused, so the run is left out rather than drawn from stale texture,
    /// and the pass is not a whole frame any more.
    fn place(&mut self, canvas: &mut Canvas, line: &Arc<ShapedTextHandle>, origin: Point) {
        if canvas.push_shaped_text(line, origin) {
            self.used.push(line.clone());
        } else {
            self.whole = false;
        }
    }
}

/// One frame's drawing, ready to be built into a canvas.
struct Frame<'a> {
    app: &'a mut App,
    bounds: Rect,
    colors: TimelineColors,
    axis_font: f32,
    marker_font: f32,
    border_width: f32,
    translate: &'a Translate,
    precision: &'a DisplayPrecision,
    now: i64,
    snapshot: &'a Arc<TimelineSnapshot>,
    buckets: &'a [TimelineBucket],
    geometry: &'a TimelineGeometry,
    labels: (Option<Arc<ShapedTextHandle>>, Option<Arc<ShapedTextHandle>>),
    hovered: Option<(Side, usize)>,
}

impl Frame<'_> {
    /// Draws one frame of a timeline into a canvas.
    ///
    /// Commands below the connectors first: a bucket whose two sides meet at the axis still
    /// shows a continuous join under both markers.
    fn paint(self, cache: &mut PaintCache) -> (Canvas, bool, Vec<Arc<ShapedTextHandle>>) {
        let Self {
            app,
            bounds,
            colors,
            axis_font,
            marker_font,
            border_width,
            translate,
            precision,
            now,
            snapshot,
            buckets,
            geometry,
            labels,
            hovered,
        } = self;
        let origin = bounds.origin;
        let mut canvas = Canvas::new(bounds);
        // The masked drawing the reference did: the frame is clipped to the box the node ended
        // up with, so a rounding error cannot paint over a sibling.
        let _ = canvas.push_clip(ClipRect {
            rect: bounds,
            corner_radius: [0.0; 4],
        });
        let mut text = TextPainter {
            app,
            cache,
            used: Vec::with_capacity(buckets.len() * 2 + TOOLTIP_VISIBLE_ROWS * 3),
            whole: true,
        };

        for tick in [geometry.axis, geometry.start_tick, geometry.end_tick] {
            canvas.push_quad(solid_quad(tick.translate(origin), colors.axis));
        }
        for marker in geometry.markers.iter() {
            canvas.push_quad(solid_quad(marker.connector.translate(origin), colors.axis));
        }

        for marker in geometry.markers.iter() {
            let fill = match marker.side {
                Side::Buy => colors.up,
                Side::Sell => colors.down,
            };
            let radius = (marker.bounds.size.width * MARKER_RADIUS_RATIO)
                .min(marker.bounds.size.width / 2.0);
            canvas.push_quad(Quad {
                bounds: marker.bounds.translate(origin),
                corner_radius: [radius; 4],
                paint: QuadPaint::Solid {
                    fill,
                    border: Some(Border {
                        width: border_width,
                        color: colors.divider,
                    }),
                },
            });
        }

        let letter_style = text_style(marker_font, colors.marker_text);
        for marker in geometry.markers.iter() {
            let letter = match marker.side {
                Side::Buy => "B",
                Side::Sell => "S",
            };
            let Some(line) = text.line(letter, &letter_style) else {
                continue;
            };
            let size = line.size();
            let center = Point::new(
                marker.bounds.origin.x + (marker.bounds.size.width - size.width) / 2.0,
                marker.bounds.origin.y + (marker.bounds.size.height - size.height) / 2.0,
            );
            text.place(&mut canvas, &line, center + origin);
        }

        if let Some(start) = &labels.0 {
            let size = start.size();
            let position = Point::new(
                geometry.axis.origin.x - AXIS_LABEL_GAP - size.width,
                geometry.axis.origin.y - size.height / 2.0,
            );
            text.place(&mut canvas, start, position + origin);
        }
        if let Some(end) = &labels.1 {
            let size = end.size();
            let position = Point::new(
                geometry.axis.max().x + AXIS_LABEL_GAP,
                geometry.axis.origin.y - size.height / 2.0,
            );
            text.place(&mut canvas, end, position + origin);
        }

        if let Some((side, index)) = hovered
            && let Some(bucket) = buckets
                .iter()
                .find(|bucket| bucket.side == side && bucket.index == index)
        {
            TooltipFrame {
                bounds,
                colors,
                border_width,
                font: axis_font,
                geometry,
                snapshot,
                precision,
                translate,
                now,
                side,
                bucket,
            }
            .paint(&mut canvas, &mut text);
        }

        let _ = canvas.pop_clip();
        (canvas, text.whole, text.used)
    }
}

/// The panel one hovered marker opens, drawn over the axis.
struct TooltipFrame<'a> {
    bounds: Rect,
    colors: TimelineColors,
    border_width: f32,
    font: f32,
    geometry: &'a TimelineGeometry,
    snapshot: &'a TimelineSnapshot,
    precision: &'a DisplayPrecision,
    translate: &'a Translate,
    now: i64,
    side: Side,
    bucket: &'a TimelineBucket,
}

impl TooltipFrame<'_> {
    fn paint(self, canvas: &mut Canvas, text: &mut TextPainter<'_>) {
        let Some(marker) = self.geometry.marker(self.side, self.bucket.index) else {
            return;
        };
        let rows = tooltip_rows(self.snapshot, self.bucket, self.precision);
        let limit = tooltip_row_limit(self.bounds.size.height);
        let shown = rows.len().min(limit);
        if shown == 0 {
            return;
        }
        let panel = tooltip_panel(
            marker,
            self.bounds.size.width,
            self.bounds.size.height,
            shown,
        );
        let origin = self.bounds.origin;
        canvas.push_quad(Quad {
            bounds: panel.translate(origin),
            corner_radius: [TOOLTIP_CORNER_RADIUS; 4],
            paint: QuadPaint::Solid {
                fill: self.colors.panel,
                border: Some(Border {
                    width: self.border_width,
                    color: self.colors.divider,
                }),
            },
        });
        let _ = canvas.push_clip(ClipRect {
            rect: panel.translate(origin),
            corner_radius: [0.0; 4],
        });

        let quantity_style = text_style(self.font, self.colors.neutral);
        let muted_style = text_style(self.font, self.colors.muted);
        let right = origin.x + panel.max().x - TOOLTIP_PADDING;
        for (offset, row) in rows.iter().take(shown).enumerate() {
            let top =
                origin.y + panel.origin.y + TOOLTIP_PADDING + offset as f32 * TOOLTIP_ROW_HEIGHT;
            let quantity = format_smolstr!(
                "{} @ {}",
                row.quantity_text(self.precision, self.translate),
                row.price_text(self.precision, self.translate)
            );
            let (label, tone) = fill_label(row, self.side, self.translate);
            let label_style = text_style(self.font, self.colors.tone(tone));
            let age = age_text(self.now, row.timestamp, self.translate);

            let Some(quantity_line) = text.line(&quantity, &quantity_style) else {
                continue;
            };
            let line_height = quantity_line.size().height;
            let line_top = top + (TOOLTIP_ROW_HEIGHT - line_height) / 2.0;
            text.place(
                canvas,
                &quantity_line,
                Point::new(origin.x + panel.origin.x + TOOLTIP_PADDING, line_top),
            );

            let age_line = text.line(&age, &muted_style);
            let label_line = text.line(&label, &label_style);
            let age_width = age_line.as_ref().map_or(0.0, |line| line.size().width);
            let label_width = label_line.as_ref().map_or(0.0, |line| line.size().width);
            if let Some(line) = &age_line {
                text.place(canvas, line, Point::new(right - age_width, line_top));
            }
            if let Some(line) = &label_line {
                text.place(
                    canvas,
                    line,
                    Point::new(right - age_width - AXIS_LABEL_GAP - label_width, line_top),
                );
            }
        }

        let _ = canvas.pop_clip();
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{node::tree::NodeKind, test_support::record::PrimitiveKind};
    use widget::test_support::WidgetTestApp;

    use super::*;
    use crate::test_support::{
        address, empty_timeline, fill, listing_key, perpetual_route, precision_for, test_routes,
    };

    /// The listing every test fill was traded on.
    fn listing() -> VenueListingKey {
        listing_key(&perpetual_route(), "ETH")
    }

    fn snapshot(fills: Vec<HyperliquidUserFill>) -> TimelineSnapshot {
        TimelineSnapshot {
            window_start: 0,
            window_end: WINDOW_MILLIS,
            fills: fills.into(),
        }
    }

    fn precision() -> DisplayPrecision {
        precision_for(test_routes())
    }

    // ── the pure behaviour the old module specified ─────────────────────────

    #[test]
    fn each_fill_is_assigned_to_one_side_bucket() {
        let snapshot = snapshot(vec![
            fill(&address("1"), &listing(), Side::Buy, "1", "1", 1, 1),
            fill(
                &address("1"),
                &listing(),
                Side::Sell,
                "1",
                "1",
                WINDOW_MILLIS,
                2,
            ),
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
            fills: vec![
                fill(&address("1"), &listing(), Side::Buy, "1", "1", 99, 1),
                fill(&address("1"), &listing(), Side::Sell, "1", "1", 201, 2),
            ]
            .into(),
        };

        assert!(build_buckets(&snapshot, 560.0).is_empty());
    }

    #[test]
    fn tooltip_rows_are_newest_first() {
        let snapshot = snapshot(vec![
            fill(&address("1"), &listing(), Side::Buy, "1", "1", 1, 1),
            fill(&address("1"), &listing(), Side::Buy, "1", "1", 2, 2),
        ]);
        let bucket = build_buckets(&snapshot, 560.0)
            .into_iter()
            .find(|bucket| bucket.side == Side::Buy)
            .expect("a buy bucket should exist");

        let rows = tooltip_rows(&snapshot, &bucket, &precision());

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
        let mut fill = fill(&address("1"), &listing(), Side::Buy, "1", "1", 1, 1);

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
                .map(|index| {
                    fill(
                        &address("1"),
                        &listing(),
                        Side::Buy,
                        "1",
                        "1",
                        index,
                        index as u64,
                    )
                })
                .collect::<Vec<_>>(),
        );
        let bucket = build_buckets(&snapshot, 560.0)
            .into_iter()
            .find(|bucket| bucket.side == Side::Buy)
            .expect("a buy bucket should exist");

        assert_eq!(tooltip_rows(&snapshot, &bucket, &precision()).len(), 8);
        assert!(tooltip_rows(&snapshot, &bucket, &precision()).len() > TOOLTIP_VISIBLE_ROWS);
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
        assert!(buys[1].bounds.origin.x - buys[0].bounds.max().x >= MARKER_GAP - 0.01);
        assert!(
            geometry
                .markers
                .iter()
                .filter(|marker| marker.side == Side::Buy)
                .all(|marker| marker.connector.size.height > 0.0)
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
                    pair[1].bounds.origin.x - pair[0].bounds.max().x >= MARKER_GAP - 0.01,
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

        assert_eq!(buy.bounds.origin.x, sell.bounds.origin.x);
        assert!(buy.bounds.max().y < sell.bounds.origin.y);
    }

    #[test]
    fn narrow_slots_use_available_space_to_keep_markers_separate() {
        let size = marker_size(20.0);

        assert_eq!(size, 14.0);
        assert!(20.0 - size >= MARKER_GAP);
    }

    #[test]
    fn geometry_cache_reuses_the_same_snapshot_and_size() {
        let snapshot = snapshot(vec![fill(
            &address("1"),
            &listing(),
            Side::Buy,
            "1",
            "1",
            1,
            1,
        )]);
        let buckets: Arc<[TimelineBucket]> = build_buckets(&snapshot, 472.0).into();
        let mut cache = PaintCache::default();

        let first = cache.geometry(&buckets, 472.0, 48.0);
        let second = cache.geometry(&buckets, 472.0, 48.0);

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn geometry_cache_rebuilds_when_the_plot_width_changes() {
        let snapshot = snapshot(vec![fill(
            &address("1"),
            &listing(),
            Side::Buy,
            "1",
            "1",
            1,
            1,
        )]);
        let buckets: Arc<[TimelineBucket]> = build_buckets(&snapshot, 472.0).into();
        let mut cache = PaintCache::default();

        let first = cache.geometry(&buckets, 472.0, 48.0);
        let second = cache.geometry(&buckets, 576.0, 48.0);

        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn data_uses_arc_identity_for_snapshot_noop_detection() {
        let snapshot = Arc::new(snapshot(vec![fill(
            &address("1"),
            &listing(),
            Side::Buy,
            "1",
            "1",
            1,
            1,
        )]));
        let mut data = TimelineData::default();

        assert!(data.replace_snapshot(Some(&snapshot)));
        assert!(!data.replace_snapshot(Some(&snapshot.clone())));
        assert!(!data.replace_snapshot(Some(&snapshot)));
        assert!(data.replace_snapshot(None));
        assert!(!data.replace_snapshot(None));
    }

    #[test]
    fn a_dimension_change_rebuckets_the_fills() {
        let snapshot = Arc::new(snapshot(vec![fill(
            &address("1"),
            &listing(),
            Side::Buy,
            "1",
            "1",
            1,
            1,
        )]));
        let mut data = TimelineData::default();
        data.replace_snapshot(Some(&snapshot));
        let before = data.buckets.clone();

        assert!(data.set_plot_width(320.0));
        assert!(
            !Arc::ptr_eq(&before, &data.buckets),
            "the fills are assigned to the slots of the new plot width"
        );
        assert!(
            !data.set_plot_width(320.0),
            "the same width is not a change"
        );
        assert_eq!(data.buckets.len(), 1);
    }

    #[test]
    fn dimension_changes_rebucket_when_a_small_delta_crosses_a_bucket_boundary() {
        let snapshot = Arc::new(snapshot(vec![fill(
            &address("1"),
            &listing(),
            Side::Buy,
            "1",
            "1",
            1,
            1,
        )]));
        let mut data = TimelineData::default();
        data.replace_snapshot(Some(&snapshot));

        assert!(data.set_plot_width(575.9));
        assert_eq!(bucket_count(data.plot_width), 23);
        assert!(data.set_plot_width(576.1));
        assert_eq!(bucket_count(data.plot_width), 24);
    }

    #[test]
    fn a_non_finite_dimension_should_not_poison_the_plot_width() {
        let mut data = TimelineData::default();
        assert!(data.set_plot_width(560.0));

        assert!(data.set_plot_width(f32::NAN));
        assert_eq!(data.plot_width, 1.0);
        assert_eq!(bucket_count(data.plot_width), 1);
    }

    // ── the canvas: what one frame actually paints ──────────────────────────

    /// One timeline drawn into a page of a fixed size, the way a list row lays it out.
    struct TimelineHarness {
        app: WidgetTestApp,
        node: NodeId,
        paint: TimelinePaint,
        precision: DisplayPrecision,
        snapshot: Option<Arc<TimelineSnapshot>>,
    }

    /// The translator the product ships: what these tests assert is the catalogue's copy.
    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            std::rc::Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    impl TimelineHarness {
        fn sync(&mut self, hover: Option<Point>) {
            let Self {
                app,
                paint,
                precision,
                snapshot,
                ..
            } = self;
            let language = app.services().language;
            app.with_cx(|cx| paint.sync(cx, snapshot.as_ref(), precision, language, hover))
                .expect("the timeline syncs");
        }

        fn canvas(&self) -> Option<Arc<Canvas>> {
            self.app.canvas_handle(self.node)
        }

        fn primitives(&self) -> Vec<widget::test_support::CanvasPrimitive> {
            self.app.canvas_primitives(self.node)
        }

        fn change_appearance(&mut self, appearance: base_ui::theme::appearance::Appearance) {
            let theme = self.app.services().theme.clone();
            assert!(
                theme.set_appearance(self.app.app_mut(), appearance),
                "the appearance switch is a change"
            );
        }
    }

    fn page(app: &mut WidgetTestApp, width: f32, height: f32) -> NodeId {
        app.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_HYPERLIQUID_WALLET_TIMELINE_PAGE")
                .style(
                    StyleBuilder::new()
                        .width(width)
                        .height(height)
                        .flex_col()
                        .build(),
                )
                .build(cx.tree_mut(), root)
                .expect("the page builds")
        })
    }

    /// Builds one timeline over a page, lays it out, and paints the first frame.
    fn harness(snapshot: Option<Arc<TimelineSnapshot>>) -> TimelineHarness {
        let mut app = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let window_page = page(&mut app, 720.0, HEIGHT);
        let node = app
            .with_cx(|cx| {
                let theme = cx.theme().clone();
                build_canvas(cx.tree_mut(), window_page, theme)
            })
            .expect("the canvas builds");
        let mut harness = TimelineHarness {
            app,
            node,
            paint: TimelinePaint::new(node),
            precision: precision(),
            snapshot,
        };
        // The first frame lays the page (and the canvas inside it) out; the sync after it is
        // the frame that paints the box the canvas was measured for.
        harness.app.drain();
        harness.sync(None);
        harness.app.drain();
        harness
    }

    fn a_timeline() -> TimelineHarness {
        harness(Some(Arc::new(snapshot(vec![
            fill(&address("1"), &listing(), Side::Buy, "1", "100", 1, 1),
            fill(&address("1"), &listing(), Side::Sell, "2", "101", 2, 2),
            fill(&address("1"), &listing(), Side::Buy, "1", "100", 2, 3),
        ]))))
    }

    fn quads(harness: &TimelineHarness) -> Vec<Rect> {
        harness
            .primitives()
            .into_iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::Quad)
            .filter_map(|primitive| primitive.bounds)
            .collect()
    }

    fn glyph_runs(harness: &TimelineHarness) -> usize {
        harness
            .primitives()
            .iter()
            .filter(|primitive| primitive.kind == PrimitiveKind::GlyphRun)
            .count()
    }

    #[test]
    fn the_canvas_node_carries_the_private_debug_id() {
        let harness = harness(None);

        assert_eq!(
            harness.app.tree().debug_id(harness.node),
            Some(TIMELINE_SELECTOR)
        );
        assert!(matches!(
            harness.app.tree().get(harness.node).expect("node").kind(),
            NodeKind::Canvas(_)
        ));
    }

    #[test]
    fn a_canvas_that_was_never_written_paints_no_primitives() {
        let mut app = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let page = page(&mut app, 720.0, HEIGHT);
        let node = app
            .with_cx(|cx| {
                let theme = cx.theme().clone();
                build_canvas(cx.tree_mut(), page, theme)
            })
            .expect("the canvas builds");
        app.drain();

        // The node was laid out, but no frame wrote a canvas: the node's own paint is still the
        // one opaque canvas entry, and there is nothing inside it.
        assert!(app.canvas_handle(node).is_some());
        assert!(app.canvas_primitives(node).is_empty());
        assert!(
            app.paint_entries(node)
                .iter()
                .any(|entry| entry.kind == PrimitiveKind::Canvas),
            "the self-drawn node is still an opaque canvas to the frame"
        );
    }

    #[test]
    fn a_steady_timeline_is_not_rewritten_and_asks_for_no_more_frames() {
        let mut harness = a_timeline();
        let before = harness.canvas().expect("the timeline drew a canvas");
        let requests = harness.app.frame_requests();

        harness.sync(None);
        harness.app.drain();

        assert!(!harness.app.rendered(), "a steady frame paints nothing");
        assert_eq!(harness.app.frame_requests(), requests);
        assert!(
            Arc::ptr_eq(
                &before,
                &harness.canvas().expect("the timeline drew a canvas")
            ),
            "a steady frame keeps the canvas it is drawn from"
        );
    }

    #[test]
    fn a_timeline_should_reshape_its_text_when_the_atlas_lease_is_gone() {
        let mut harness = a_timeline();
        let before = harness.canvas().expect("the timeline drew a canvas");

        // An atlas reset under the same font stack: every handle the canvas was painted from is
        // leased from an atlas that no longer exists, while the glyphs would shape back the same.
        // The engine drops runs whose lease is gone at encode time, so the timeline owes a frame
        // even though nothing a reader asked for moved.
        let stack = harness.app.app().font_stack().clone();
        harness.app.app_mut().set_font_stack(stack);

        harness.sync(None);
        harness.app.drain();

        assert!(
            !Arc::ptr_eq(
                &before,
                &harness.canvas().expect("the timeline drew a canvas")
            ),
            "a stale glyph lease is a canvas the timeline owed"
        );
    }

    #[test]
    fn switching_the_appearance_repaints_the_timeline_in_the_new_roles() {
        let mut harness = a_timeline();
        let light = harness.canvas().expect("the timeline drew a canvas");
        let theme = harness.app.services().theme.clone();
        let requests = harness.app.frame_requests();

        harness.change_appearance(base_ui::theme::appearance::Appearance::Dark);
        assert!(
            harness.app.frame_requests() > requests,
            "an appearance switch invalidates the canvas node and schedules the frame"
        );
        harness.sync(None);
        harness.app.drain();

        let dark = harness.canvas().expect("the timeline drew a canvas");
        assert!(
            !Arc::ptr_eq(&light, &dark),
            "the redraw prepared a canvas of its own"
        );
        let expected = theme
            .theme_for(base_ui::theme::appearance::Appearance::Dark)
            .role(Role::MarketUp);
        assert!(
            harness
                .primitives()
                .iter()
                .any(|primitive| primitive.color == Some(expected)),
            "the mark of a buy bucket is painted in the dark role"
        );
    }

    #[test]
    fn a_snapshot_with_no_fills_still_draws_the_axis() {
        let harness = harness(Some(empty_timeline()));

        let axis_quads = quads(&harness);
        assert_eq!(axis_quads.len(), 3, "the axis line and its two ticks");
        assert!(glyph_runs(&harness) >= 2, "the two end labels are drawn");
    }

    #[test]
    fn a_marker_holds_the_pointer_and_draws_its_fills() {
        let mut harness = harness(Some(Arc::new(snapshot(
            (0..8)
                .map(|index| {
                    fill(
                        &address("1"),
                        &listing(),
                        Side::Buy,
                        "1",
                        "100",
                        index,
                        index as u64,
                    )
                })
                .collect::<Vec<_>>(),
        ))));
        let before = harness.primitives().len();
        // One bucket holds all eight fills: the oldest is at the left of the plot.
        let marker = harness
            .paint
            .cache
            .geometry
            .as_ref()
            .expect("the geometry is cached")
            .geometry
            .marker(Side::Buy, 0)
            .copied()
            .expect("the buy bucket has a marker");
        let point = Point::new(
            marker.bounds.origin.x + marker.bounds.size.width / 2.0,
            marker.bounds.origin.y + marker.bounds.size.height / 2.0,
        );

        harness.sync(Some(point));
        harness.app.drain();

        let primitives = harness.primitives();
        assert!(
            primitives.len() > before,
            "the panel is drawn over the axis"
        );
        assert!(
            primitives.iter().any(|primitive| {
                primitive.color
                    == Some(
                        harness
                            .app
                            .services()
                            .theme
                            .theme_for(base_ui::theme::appearance::Appearance::Light)
                            .role(Role::SurfaceOverlay),
                    )
            }),
            "the panel is the theme's overlay surface"
        );
        assert_eq!(
            glyph_runs(&harness),
            2 + 1 + TOOLTIP_VISIBLE_ROWS * 3,
            "the two labels, the marker's letter, and three lines for each of the newest six fills"
        );

        // Moving off the marker and its panel closes it.
        harness.sync(Some(Point::new(1.0, 1.0)));
        harness.app.drain();
        assert_eq!(
            glyph_runs(&harness),
            2 + 1,
            "only the labels and the marker's letter are left"
        );
    }

    #[test]
    fn switching_the_language_repaints_the_labels() {
        let mut harness = a_timeline();
        let english = harness.canvas().expect("the timeline drew a canvas");

        harness.app.switch_language(LanguageCode::ZhCn);
        harness.sync(None);
        harness.app.drain();

        assert!(
            !Arc::ptr_eq(
                &english,
                &harness.canvas().expect("the timeline drew a canvas")
            ),
            "the labels are shaped in the language showing now"
        );
        // A line is one glyph run per font it needs, and the Chinese start label mixes a digit
        // with CJK, so the count is a lower bound rather than the English frame's count.
        assert!(
            glyph_runs(&harness) >= 2 + 2,
            "the two labels and the two marker letters are drawn in the new language"
        );
    }

    #[test]
    fn a_canvas_that_the_layout_has_not_measured_is_left_alone() {
        let mut app = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let page = page(&mut app, 720.0, HEIGHT);
        let node = app
            .with_cx(|cx| {
                let theme = cx.theme().clone();
                build_canvas(cx.tree_mut(), page, theme)
            })
            .expect("the canvas builds");
        let mut paint = TimelinePaint::new(node);
        let snapshot = Arc::new(snapshot(vec![fill(
            &address("1"),
            &listing(),
            Side::Buy,
            "1",
            "100",
            1,
            1,
        )]));
        let precision = precision();
        let language = app.services().language;

        // No frame has been drained when the sync runs: the node has no box yet, so nothing is
        // written and the next sync is what paints it.
        app.with_cx(|cx| paint.sync(cx, Some(&snapshot), &precision, language, None))
            .expect("the timeline syncs");
        app.drain();

        assert!(app.canvas_primitives(node).is_empty());
    }

    #[test]
    fn releasing_the_timeline_reclaims_its_caches_and_value_slots() {
        let mut harness = a_timeline();
        let baseline = harness.app.signal_slot_stats();
        let geometry = harness.app.app().geometry().len();
        let drawn = harness.canvas().expect("the timeline drew a canvas");

        {
            let TimelineHarness { app, paint, .. } = &mut harness;
            app.with_cx(|cx| paint.release(cx));
        }

        assert_eq!(
            harness.app.app().geometry().len(),
            geometry,
            "the timeline registers no geometry to hand back"
        );
        assert_eq!(
            harness.app.signal_slot_stats().live,
            baseline.live,
            "the timeline mints no value slots"
        );
        assert!(harness.paint.prepared.is_none());
        assert!(harness.paint.painted_text.is_empty());

        // A released timeline draws nothing more, even if the pane asks once more.
        harness.sync(None);
        harness.app.drain();
        assert!(Arc::ptr_eq(
            &drawn,
            &harness
                .canvas()
                .expect("the row's node still holds its last canvas")
        ));
    }
}
