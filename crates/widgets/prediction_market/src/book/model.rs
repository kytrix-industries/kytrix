//! The grouped prediction order book the panel draws.
//!
//! A venue publishes its book at its own tick and with as many levels as it
//! likes; a pane draws one line per grouped price and a few rows of each side.
//! Everything here is pure: buckets, cumulative notional, cumulative depth and
//! the labels those numbers are read as. No element, no theme, no window.

use std::collections::BTreeMap;

use base_ui::format::group_digits;
use market_participants_model::{
    decimal::{Fraction, Price, Quantity, Ratio, Tick},
    prediction::{PredictionBookLevel, PredictionOrderBook},
};
use smol_str::{SmolStr, format_smolstr};

/// The price groupings the book offers, as multiples of the venue tick.
pub(crate) const GROUPING_MULTIPLIERS: [u32; 3] = [1, 10, 100];

/// The smallest share of the body the book panel may take.
pub(crate) const MIN_BOOK_RATIO: f32 = 0.2;

/// The largest share of the body the book panel may take.
pub(crate) const MAX_BOOK_RATIO: f32 = 0.8;

/// The share of the body the book panel takes when nothing valid was stored.
pub(crate) const DEFAULT_BOOK_RATIO: f32 = 0.5;

/// The fixed-point scale of a [`Fraction`], read from the model itself.
const FRACTION_SCALE: u128 = Fraction::ONE.ratio().inner();

/// One grouped price level of one side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GroupedLevel {
    /// The bucket's price: the tick-aligned boundary the levels in it round to.
    pub price: Fraction,
    /// The shares resting at the bucket, summed over the levels it covers.
    pub size: Quantity,
    /// The notional of this row and everything better than it on its side.
    pub total: Quantity,
    /// How much of its side's resting shares this row and the better ones hold,
    /// in `0..1`.
    pub depth: f32,
}

/// One grouped reading of a prediction order book.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GroupedBook {
    /// The ask side, best (lowest) price first.
    pub asks: Vec<GroupedLevel>,
    /// The bid side, best (highest) price first.
    pub bids: Vec<GroupedLevel>,
    pub last: Option<Fraction>,
    /// The best ask minus the best bid, when both exist and the book is not
    /// crossed.
    pub spread: Option<Fraction>,
    /// The venue's own tick.
    pub tick: Tick,
    pub best_bid: Option<Fraction>,
    pub best_ask: Option<Fraction>,
}

impl GroupedBook {
    /// Whether both sides are empty, which is what the panel calls "no data".
    ///
    /// A book can be published with no resting level at all, and it is still the
    /// pane's content: the panel says so instead of drawing an empty ladder.
    pub(crate) fn is_empty(&self) -> bool {
        self.best_bid.is_none() && self.best_ask.is_none()
    }
}

/// Which side a bucket is rounded for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Ask,
    Bid,
}

/// Groups one book at `multiplier` times its tick.
///
/// `multiplier` is settled against what this book's tick actually offers, so a
/// caller that stored a grouping the venue's tick cannot express draws the
/// coarsest valid one instead.
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

/// The groupings this tick can express: a step has to be smaller than a whole
/// share to be a price grouping at all.
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

/// The multiplier a book is actually grouped at.
///
/// A stored grouping is not a promise that this venue's tick can express it: a
/// step of one share or more is not a price, so such a choice goes back to the
/// finest one instead of being drawn as a single bucket.
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

/// The notch the panel takes of the body, kept inside what a splitter may leave.
pub(crate) fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_BOOK_RATIO, MAX_BOOK_RATIO)
    } else {
        DEFAULT_BOOK_RATIO
    }
}

/// The step a book at this tick and multiplier is grouped by.
fn grouping_step(tick: Tick, multiplier: u32) -> Tick {
    let multiplier = settle_grouping(tick, multiplier);
    let inner = tick.inner().saturating_mul(u128::from(multiplier));
    Tick::from_inner(inner).unwrap_or(tick)
}

/// Groups one side's levels, best price first.
fn group_side(levels: &[PredictionBookLevel], side: Side, step: Tick) -> Vec<GroupedLevel> {
    let step = step.inner();
    let mut buckets: BTreeMap<Ratio, Quantity> = BTreeMap::new();
    for level in levels {
        // A zero size is a removed level, not a bucket of no shares.
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
    // Ask buckets walk away from the best price upwards, bid buckets downwards,
    // so reversing the ascending map is what makes both sides best-first.
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
        // Notional is computed in the model's own fixed point: a float sum would
        // drift away from the number the venue published.
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

/// Rounds a price to the tick-aligned boundary its bucket is named by.
///
/// Venue levels are multiples of the tick, so a bucket boundary is exact; the
/// rounding direction is still stated so a level that is not is bucketed with
/// its neighbours rather than skipped: bids round down, asks round up, which is
/// the side that never makes a bucket look better than the level inside it.
fn bucket_price(price: u128, step: u128, side: Side) -> u128 {
    match side {
        Side::Bid => price / step * step,
        Side::Ask => price.div_ceil(step).saturating_mul(step),
    }
}

/// One row's share of its side's resting shares.
fn depth_ratio(cumulative: u128, total: u128) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (cumulative as f64 / total as f64).clamp(0.0, 1.0) as f32
}

/// A price, or a difference of prices, in cents at the precision its tick is
/// quoted with.
///
/// The venue quotes a price as a fraction of one share; a reader of a prediction
/// book reads it in cents, and the tick decides how many decimals survive the
/// conversion: a tick of `0.001` is a tenth of a cent, one of `0.01` is a whole
/// cent. A spread is a difference of prices, so it is quoted the same way one is.
pub(crate) fn format_cents(value: Fraction, tick: Tick) -> SmolStr {
    let places = tick.decimal_places().saturating_sub(2);
    format_smolstr!(
        "{}¢",
        format_scaled(value.ratio().inner().saturating_mul(100), places)
    )
}

/// A share count, grouped in threes and rounded to two decimals.
pub(crate) fn format_shares(size: Quantity) -> SmolStr {
    format_scaled(size.inner(), 2)
}

/// A quote-currency notional, grouped in threes and rounded to two decimals.
pub(crate) fn format_notional(total: Quantity) -> SmolStr {
    format_smolstr!("${}", format_scaled(total.inner(), 2))
}

/// Renders a fixed-point value with `places` decimals and grouped integer digits.
fn format_scaled(inner: u128, places: usize) -> SmolStr {
    let Some(tick) = tick_with_places(places) else {
        return SmolStr::new_static("--");
    };
    group_digits(&Price::from_inner(inner).format_and_round_tick(tick))
}

/// The tick that states exactly `places` decimals, which is how the model's own
/// formatter is asked for a fixed precision.
fn tick_with_places(places: usize) -> Option<Tick> {
    let exponent = u32::try_from(18_usize.checked_sub(places)?).ok()?;
    Tick::from_inner(10_u128.checked_pow(exponent)?).ok()
}
