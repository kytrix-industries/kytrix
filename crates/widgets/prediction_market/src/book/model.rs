use std::collections::BTreeMap;

use base_ui::kit::format::group_digits;
use market_participants_model::{
    decimal::{Fraction, Price, Quantity, Ratio, Tick},
    prediction::{PredictionBookLevel, PredictionOrderBook},
};
use smol_str::{SmolStr, format_smolstr};

pub(crate) const GROUPING_MULTIPLIERS: [u32; 3] = [1, 10, 100];

pub(crate) const MIN_BOOK_RATIO: f32 = 0.2;

pub(crate) const MAX_BOOK_RATIO: f32 = 0.8;

pub(crate) const DEFAULT_BOOK_RATIO: f32 = 0.5;

const FRACTION_SCALE: u128 = Fraction::ONE.ratio().inner();

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GroupedLevel {
    pub price: Fraction,
    pub size: Quantity,
    pub total: Quantity,
    pub depth: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GroupedBook {
    pub asks: Vec<GroupedLevel>,
    pub bids: Vec<GroupedLevel>,
    pub last: Option<Fraction>,
    pub spread: Option<Fraction>,
    pub tick: Tick,
    pub best_bid: Option<Fraction>,
    pub best_ask: Option<Fraction>,
}

impl GroupedBook {
    pub(crate) fn is_empty(&self) -> bool {
        self.best_bid.is_none() && self.best_ask.is_none()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Ask,
    Bid,
}

pub(crate) fn group(book: &PredictionOrderBook, multiplier: u32) -> GroupedBook {
    let tick = book.tick_size;
    let step = grouping_step(tick, multiplier);
    let asks = group_side(&book.asks, Side::Ask, step);
    let bids = group_side(&book.bids, Side::Bid, step);
    let best_ask = asks.first().map(|level| level.price);
    let best_bid = bids.first().map(|level| level.price);
    let spread = best_ask.zip(best_bid).and_then(|(ask, bid)| {
        let difference = ask.ratio().inner().checked_sub(bid.ratio().inner())?;
        Fraction::new(Ratio::from_inner(difference)).ok()
    });
    GroupedBook {
        asks,
        bids,
        last: book.last_trade_price,
        spread,
        tick,
        best_bid,
        best_ask,
    }
}

pub(crate) fn grouping_steps(tick: Tick) -> Vec<(u32, SmolStr)> {
    let mut steps = Vec::with_capacity(GROUPING_MULTIPLIERS.len());
    for multiplier in GROUPING_MULTIPLIERS {
        let Some(inner) = tick.inner().checked_mul(u128::from(multiplier)) else {
            continue;
        };
        if inner >= FRACTION_SCALE {
            continue;
        }
        let Ok(step) = Tick::from_inner(inner) else {
            continue;
        };
        steps.push((multiplier, format_smolstr!("{step}")));
    }
    steps
}

pub(crate) fn settle_grouping(tick: Tick, multiplier: u32) -> u32 {
    let offered = grouping_steps(tick);
    if offered
        .iter()
        .any(|(candidate, _)| *candidate == multiplier)
    {
        multiplier
    } else {
        GROUPING_MULTIPLIERS[0]
    }
}

pub(crate) fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_BOOK_RATIO, MAX_BOOK_RATIO)
    } else {
        DEFAULT_BOOK_RATIO
    }
}

fn grouping_step(tick: Tick, multiplier: u32) -> Tick {
    let multiplier = settle_grouping(tick, multiplier);
    let inner = tick.inner().saturating_mul(u128::from(multiplier));
    Tick::from_inner(inner).unwrap_or(tick)
}

fn group_side(levels: &[PredictionBookLevel], side: Side, step: Tick) -> Vec<GroupedLevel> {
    let step = step.inner();
    let mut buckets: BTreeMap<Ratio, Quantity> = BTreeMap::new();
    for level in levels {
        if level.size.is_zero() {
            continue;
        }
        let price = Ratio::from_inner(bucket_price(level.price.ratio().inner(), step, side));
        let size = buckets.entry(price).or_insert(Quantity::ZERO);
        *size = Quantity::from_inner(size.inner().saturating_add(level.size.inner()));
    }
    let total_shares = buckets
        .values()
        .fold(0_u128, |total, size| total.saturating_add(size.inner()));
    let mut ordered: Vec<(Ratio, Quantity)> = buckets.into_iter().collect();
    if side == Side::Bid {
        ordered.reverse();
    }
    let mut grouped = Vec::with_capacity(ordered.len());
    let mut cumulative_shares = 0_u128;
    let mut cumulative_notional = Quantity::ZERO;
    for (price, size) in ordered {
        let Ok(price) = Fraction::new(price) else {
            continue;
        };
        cumulative_shares = cumulative_shares.saturating_add(size.inner());
        let notional = size
            .checked_mul_price(Price::from_inner(price.ratio().inner()))
            .unwrap_or(Quantity::ZERO);
        cumulative_notional =
            Quantity::from_inner(cumulative_notional.inner().saturating_add(notional.inner()));
        grouped.push(GroupedLevel {
            price,
            size,
            total: cumulative_notional,
            depth: depth_ratio(cumulative_shares, total_shares),
        });
    }
    grouped
}

fn bucket_price(price: u128, step: u128, side: Side) -> u128 {
    match side {
        Side::Bid => price / step * step,
        Side::Ask => price.div_ceil(step).saturating_mul(step),
    }
}

fn depth_ratio(cumulative: u128, total: u128) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (cumulative as f64 / total as f64).clamp(0.0, 1.0) as f32
}

pub(crate) fn format_cents(value: Fraction, tick: Tick) -> SmolStr {
    let places = tick.decimal_places().saturating_sub(2);
    format_smolstr!(
        "{}¢",
        format_scaled(value.ratio().inner().saturating_mul(100), places)
    )
}

pub(crate) fn format_shares(size: Quantity) -> SmolStr {
    format_scaled(size.inner(), 2)
}

pub(crate) fn format_notional(total: Quantity) -> SmolStr {
    format_smolstr!("${}", format_scaled(total.inner(), 2))
}

fn format_scaled(inner: u128, places: usize) -> SmolStr {
    let Some(tick) = tick_with_places(places) else {
        return SmolStr::new_static("--");
    };
    group_digits(&Price::from_inner(inner).format_and_round_tick(tick))
}

fn tick_with_places(places: usize) -> Option<Tick> {
    let exponent = u32::try_from(18_usize.checked_sub(places)?).ok()?;
    Tick::from_inner(10_u128.checked_pow(exponent)?).ok()
}
