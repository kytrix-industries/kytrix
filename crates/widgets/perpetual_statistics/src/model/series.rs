//! What the five statistics charts are made of.
//!
//! A venue publishes these series as a bounded history rather than as a stream,
//! so a snapshot here is a window of the most recent [`POINT_LIMIT`] intervals —
//! already sorted, already deduplicated, and already reduced to the values the
//! charts can actually draw. Everything a chart needs to render a point is
//! derived once, at this boundary, and shared through an `Arc`: the drawing code
//! never formats a number, and a snapshot that changed nothing keeps the
//! allocation it had, which is how a refresh that returns the same window is
//! recognized as no change at all.

use std::{collections::BTreeMap, sync::Arc};

use market_participants_model::{
    decimal::{Quantity, Ratio},
    error::ModelError,
    listing::rules::ListingPrecision,
    market::book::{ListingLongShortRatio, ListingOpenInterest, ListingTakerVolume},
    routing::CapabilityId,
};

/// How many intervals one chart plots.
///
/// This is the request size as well as the retained size: every venue this
/// widget supports accepts it (Binance's futures-data endpoints take 1..=500 and
/// OKX's Rubik endpoints take 1..=200), and a chart of the most recent thirty
/// intervals is what the pane can show at a readable bar width.
pub(crate) const POINT_LIMIT: u8 = 30;

/// Which series a chart plots. One per widget, and the tag that keeps a
/// snapshot from being applied to a chart that is not drawing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetricKind {
    OpenInterest,
    TopTraderAccountRatio,
    TopTraderPositionRatio,
    LongShortAccountRatio,
    TakerVolume,
}

impl MetricKind {
    /// Whether a chart of this series labels its values at the listing's own
    /// ticks. A ratio is a plain number with no listing behind it, so it never
    /// does.
    pub(crate) const fn needs_instrument_precision(self) -> bool {
        matches!(self, Self::OpenInterest | Self::TakerVolume)
    }

    /// The capability that serves this series.
    ///
    /// The five series are five capabilities of the route registry, so which one a
    /// pane needs is a property of the series rather than of "statistics" as a
    /// whole: a route that serves open interest and not taker volume is a route a
    /// taker-volume pane cannot be pointed at.
    pub(crate) const fn capability(self) -> CapabilityId {
        match self {
            Self::OpenInterest => CapabilityId::OpenInterest,
            Self::TopTraderAccountRatio => CapabilityId::TopTraderAccountRatio,
            Self::TopTraderPositionRatio => CapabilityId::TopTraderPositionRatio,
            Self::LongShortAccountRatio => CapabilityId::LongShortAccountRatio,
            Self::TakerVolume => CapabilityId::TakerVolume,
        }
    }
}

/// One open-interest interval: the size of the open interest in the listing's own
/// quantity unit, and what it was worth as a monetary amount.
///
/// The amount is optional because a venue is not obliged to publish it: the line
/// is then absent for that interval rather than drawn from a number nobody
/// published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenInterestPoint {
    pub timestamp: i64,
    pub base_asset_volume: Quantity,
    pub quote_asset_volume: Option<Quantity>,
}

/// One interval of a long/short series.
///
/// The venues hand the ratio over as the only number they agree on, so the long
/// and short shares a ratio chart draws are derived from it rather than read
/// from the payload.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LongShortRatioPoint {
    pub timestamp: i64,
    pub ratio: Ratio,
}

/// One interval of taker volume, split by the side that crossed the spread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TakerVolumePoint {
    pub timestamp: i64,
    pub buy_volume: Quantity,
    pub sell_volume: Quantity,
}

/// The points of one chart, tagged with the series they belong to.
#[derive(Clone, Debug)]
pub(crate) enum StatisticsPoints {
    OpenInterest(Arc<[OpenInterestPoint]>),
    LongShortRatio(Arc<[LongShortRatioPoint]>),
    TakerVolume(Arc<[TakerVolumePoint]>),
}

/// One complete reading of a chart: which series, at which revision, with which
/// instrument precision.
///
/// The metadata is optional because the chart is worth drawing without it: the
/// labels fall back to a compact form rather than the pane waiting for a second
/// request that may never answer.
#[derive(Clone, Debug)]
pub(crate) struct StatisticsSnapshot {
    pub revision: u64,
    pub metadata: Option<ListingPrecision>,
    pub points: StatisticsPoints,
}

/// Normalizes a fetch of open interest into the window a chart plots.
///
/// The size of the open interest is a traded quantity and its worth is a monetary
/// amount, so both are validated as what they claim to be: a reading expressed in
/// a unit the chart cannot label is refused rather than drawn as if it were one.
pub(crate) fn normalize_open_interest(
    items: impl IntoIterator<Item = ListingOpenInterest>,
    previous: Option<&StatisticsPoints>,
) -> Result<(StatisticsPoints, bool), ModelError> {
    let mut points = BTreeMap::new();
    for item in items {
        if item.timestamp <= 0 {
            continue;
        }
        item.quantity.validate_traded("open_interest.quantity")?;
        let quote_asset_volume = match &item.notional {
            Some(notional) => {
                notional.validate_amount("open_interest.notional")?;
                Some(notional.value)
            }
            None => None,
        };
        insert_latest(
            &mut points,
            item.timestamp,
            OpenInterestPoint {
                timestamp: item.timestamp,
                base_asset_volume: item.quantity.value,
                quote_asset_volume,
            },
        );
    }
    let previous = match previous {
        Some(StatisticsPoints::OpenInterest(points)) => Some(points),
        _ => None,
    };
    let (points, changed) = take_latest(points, previous);
    Ok((StatisticsPoints::OpenInterest(points), changed))
}

/// Normalizes a fetch of a long/short series into the window a chart plots.
///
/// The three long/short series are separate endpoints with the same shape, so
/// one normalizer serves all of them: which endpoint answered is the stream
/// key's business, not the window's.
pub(crate) fn normalize_long_short_ratio(
    items: impl IntoIterator<Item = ListingLongShortRatio>,
    previous: Option<&StatisticsPoints>,
) -> Result<(StatisticsPoints, bool), ModelError> {
    let mut points = BTreeMap::new();
    for item in items {
        if item.timestamp <= 0 {
            continue;
        }
        insert_latest(
            &mut points,
            item.timestamp,
            LongShortRatioPoint {
                timestamp: item.timestamp,
                ratio: item.ratio,
            },
        );
    }
    let previous = match previous {
        Some(StatisticsPoints::LongShortRatio(points)) => Some(points),
        _ => None,
    };
    let (points, changed) = take_latest(points, previous);
    Ok((StatisticsPoints::LongShortRatio(points), changed))
}

/// Normalizes a fetch of taker volume into the window a chart plots.
///
/// Both sides are traded quantities, so each is validated as one: a volume the
/// chart cannot label is refused rather than drawn as if it were a traded one.
pub(crate) fn normalize_taker_volume(
    items: impl IntoIterator<Item = ListingTakerVolume>,
    previous: Option<&StatisticsPoints>,
) -> Result<(StatisticsPoints, bool), ModelError> {
    let mut points = BTreeMap::new();
    for item in items {
        if item.timestamp <= 0 {
            continue;
        }
        item.buy.validate_traded("taker_volume.buy")?;
        item.sell.validate_traded("taker_volume.sell")?;
        insert_latest(
            &mut points,
            item.timestamp,
            TakerVolumePoint {
                timestamp: item.timestamp,
                buy_volume: item.buy.value,
                sell_volume: item.sell.value,
            },
        );
    }
    let previous = match previous {
        Some(StatisticsPoints::TakerVolume(points)) => Some(points),
        _ => None,
    };
    let (points, changed) = take_latest(points, previous);
    Ok((StatisticsPoints::TakerVolume(points), changed))
}

/// One interval per timestamp, keeping the newest [`POINT_LIMIT`] of them.
///
/// The venues are free to repeat a timestamp (a series can be revised within the
/// interval it belongs to) and are not consistent about ordering, so the input is
/// keyed rather than appended: the last value for a timestamp is the one that
/// counts.
pub(crate) fn insert_latest<T>(points: &mut BTreeMap<i64, T>, timestamp: i64, point: T) {
    let limit = usize::from(POINT_LIMIT);
    if points.len() == limit
        && !points.contains_key(&timestamp)
        && points
            .first_key_value()
            .is_some_and(|(&oldest, _)| timestamp < oldest)
    {
        return;
    }
    points.insert(timestamp, point);
    if points.len() > limit {
        points.pop_first();
    }
}

/// Turns the window into a shared slice, reusing the previous allocation when
/// the window did not change.
///
/// Reusing the allocation is what lets every layer above compare by pointer: a
/// chart that is handed the same `Arc` has nothing to redraw, and the widget has
/// nothing to report.
pub(crate) fn take_latest<T: PartialEq>(
    points: BTreeMap<i64, T>,
    previous: Option<&Arc<[T]>>,
) -> (Arc<[T]>, bool) {
    let mut latest = Vec::with_capacity(points.len());
    latest.extend(points.into_values());
    if let Some(previous) = previous
        && previous.as_ref() == latest.as_slice()
    {
        return (previous.clone(), false);
    }
    (latest.into(), true)
}

/// The long and short shares a ratio implies, if the ratio is a real one.
///
/// Only the ratio is published, and it is the long side over the short side, so
/// the two shares a stacked bar needs are that ratio resolved against a whole.
/// A ratio that is not finite and positive has no shares: the point is not a
/// reading of anything, and the chart breaks its line there rather than drawing
/// a bar that is not there.
pub(crate) fn ratio_shares(ratio: Ratio) -> Option<(f64, f64)> {
    let ratio = ratio_as_f64(ratio);
    if !ratio.is_finite() || ratio <= 0.0 {
        return None;
    }
    let long_share = ratio / (1.0 + ratio);
    (long_share.is_finite()).then_some((long_share, 1.0 - long_share))
}

/// The value of a ratio as `f64`, for the geometry a chart is built from.
pub(crate) fn ratio_as_f64(ratio: Ratio) -> f64 {
    const RATIO_SCALE: f64 = 1_000_000_000_000_000_000.0;
    ratio.inner() as f64 / RATIO_SCALE
}

/// The value of a quantity as `f64`, for the geometry a chart is built from.
pub(crate) fn quantity_as_f64(quantity: Quantity) -> f64 {
    /// The decimal scale every value in `market_participants_model` is stored at.
    const DECIMAL_SCALE: f64 = 1_000_000_000_000_000_000.0;
    quantity.inner() as f64 / DECIMAL_SCALE
}
