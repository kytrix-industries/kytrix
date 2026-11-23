use std::{collections::BTreeMap, sync::Arc};

use market_participants_model::{
    decimal::{Quantity, Ratio},
    error::ModelError,
    listing::rules::ListingPrecision,
    market::book::{ListingLongShortRatio, ListingOpenInterest, ListingTakerVolume},
    routing::CapabilityId,
};

pub(crate) const POINT_LIMIT: u8 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetricKind {
    OpenInterest,
    TopTraderAccountRatio,
    TopTraderPositionRatio,
    LongShortAccountRatio,
    TakerVolume,
}

impl MetricKind {
    pub(crate) const fn needs_instrument_precision(self) -> bool {
        matches!(self, Self::OpenInterest | Self::TakerVolume)
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenInterestPoint {
    pub timestamp: i64,
    pub base_asset_volume: Quantity,
    pub quote_asset_volume: Option<Quantity>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LongShortRatioPoint {
    pub timestamp: i64,
    pub ratio: Ratio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TakerVolumePoint {
    pub timestamp: i64,
    pub buy_volume: Quantity,
    pub sell_volume: Quantity,
}

#[derive(Clone, Debug)]
pub(crate) enum StatisticsPoints {
    OpenInterest(Arc<[OpenInterestPoint]>),
    LongShortRatio(Arc<[LongShortRatioPoint]>),
    TakerVolume(Arc<[TakerVolumePoint]>),
}

#[derive(Clone, Debug)]
pub(crate) struct StatisticsSnapshot {
    pub revision: u64,
    pub metadata: Option<ListingPrecision>,
    pub points: StatisticsPoints,
}

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

pub(crate) fn ratio_shares(ratio: Ratio) -> Option<(f64, f64)> {
    let ratio = ratio_as_f64(ratio);
    if !ratio.is_finite() || ratio <= 0.0 {
        return None;
    }
    let long_share = ratio / (1.0 + ratio);
    (long_share.is_finite()).then_some((long_share, 1.0 - long_share))
}

pub(crate) fn ratio_as_f64(ratio: Ratio) -> f64 {
    const RATIO_SCALE: f64 = 1_000_000_000_000_000_000.0;
    ratio.inner() as f64 / RATIO_SCALE
}

pub(crate) fn quantity_as_f64(quantity: Quantity) -> f64 {
    const DECIMAL_SCALE: f64 = 1_000_000_000_000_000_000.0;
    quantity.inner() as f64 / DECIMAL_SCALE
}
