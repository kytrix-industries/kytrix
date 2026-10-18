//! The order book's model: the snapshot the service publishes and the grouped
//! book the widget draws.
//!
//! A venue publishes hundreds or thousands of levels; a pane shows a few dozen
//! rows of them, grouped to a price step the user picks. The two are kept apart
//! deliberately: [`OrderBookSnapshot`] is exactly what the venue said (truncated
//! to a known depth), and [`GroupedBook`] is the derived state a frame draws.
//! Regrouping is therefore cheap to repeat — changing the price step or the row
//! limit rebuilds it from the snapshot the widget already holds, without touching
//! the subscription.
//!
//! # Why the grouping carries animation state
//!
//! A depth bar grows and shrinks; easing that movement is what makes a book
//! readable at a glance. The easing needs to know where the bar *is* right now,
//! which is neither the previous value nor the new one but the last thing the
//! frame actually painted, so every level also carries the ratio it was painted
//! with and the progress that interpolation had reached.
//!
//! # Why the labels are formatted here
//!
//! Formatting a price costs a decimal rounding plus a string allocation, and a
//! book is rebuilt several times a second. Each level therefore formats its three
//! labels at most once and then reuses them for as long as the value it shows
//! stays the same, and the book as a whole does the same for the mid price, the
//! spread and the imbalance.

use std::{collections::BTreeMap, sync::Arc};

use gpui_kit::SharedString;
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Quantity, Tick},
    listing::rules::{ListingPrecision, TradingRules},
    market::{
        book::{ListingOrderBook, ListingOrderBookLevel, MarketSequence},
        units::QuantityUnit,
    },
};
use smol_str::format_smolstr;

/// The price-grouping multipliers, in the order they are offered.
pub(super) const PRICE_MULTIPLIERS: [u32; 5] = [1, 10, 100, 1_000, 10_000];
/// The visible rows one side may be limited to, in the order they are offered.
pub(super) const WINDOW_SIZES: [usize; 3] = [25, 50, 100];
/// The visible rows a venue that publishes a shallow book is offered.
pub(super) const SHALLOW_WINDOW_SIZES: [usize; 3] = [5, 10, 20];
/// The most levels one side keeps. The far end of a book is never drawn, so a
/// venue that publishes deeper than this is truncated where nothing is read.
pub(super) const MAX_LEVELS_PER_SIDE: usize = 5_000;

/// The row limits one side of a book `depth` levels deep is offered.
///
/// A venue that publishes at most twenty levels per side cannot fill a standard
/// window at all, so its book offers the shallow ladder instead: a window larger
/// than the book would only draw the rows that exist under a label that claims
/// more. A route whose depth the venue has not answered yet keeps the standard
/// ladder, which is what a book deeper than every window looks like.
pub(super) fn window_sizes(depth: Option<u16>) -> &'static [usize] {
    match depth {
        Some(depth) if usize::from(depth) < WINDOW_SIZES[0] => &SHALLOW_WINDOW_SIZES,
        _ => &WINDOW_SIZES,
    }
}

/// The row limit a book of that depth starts with.
///
/// The standard default is kept wherever the route can fill it; a shallow route
/// starts with the deepest window it offers, which is the one closest to the
/// default without naming rows the book does not have.
pub(super) fn default_window_size(depth: Option<u16>) -> usize {
    let sizes = window_sizes(depth);
    if sizes.contains(&WINDOW_SIZES[0]) {
        WINDOW_SIZES[0]
    } else {
        sizes.last().copied().unwrap_or(WINDOW_SIZES[0])
    }
}

#[cfg(test)]
std::thread_local! {
    // Counts the labels one test formatted, so a test can prove that a rebuild of
    // unchanged levels reuses them instead of formatting them again.
    static FORMAT_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn format_shared<T>(format: impl FnOnce() -> T) -> SharedString
where
    T: AsRef<str>,
{
    #[cfg(test)]
    FORMAT_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    SharedString::new(format().as_ref())
}

/// The increments and the units every label of one book is written in, and how
/// deep the venue publishes it.
///
/// The increments and the units come from the listing's own rule set, which the
/// service reads before it opens a stream, and they are carried on the snapshot
/// because a book cannot be rounded or labelled without them. The depth is the
/// venue's own bound for the route, read at the same time: it is what decides
/// which row limits the book can fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OrderBookMetadata {
    /// The increments every price and quantity of the book is written at.
    pub precision: ListingPrecision,
    /// The asset every price of the listing is quoted in.
    pub quote_asset: AssetId,
    /// What every quantity of the listing counts.
    pub quantity_unit: QuantityUnit,
    /// The deepest book the venue publishes per side.
    pub depth: u16,
}

impl OrderBookMetadata {
    /// Reads the increments and the units of one book, with the depth its venue
    /// publishes it at.
    ///
    /// A price tick is non-zero by its own type, and a quantity step the rule set
    /// states as zero leaves the book rounding quantities to whole units rather
    /// than refusing to draw it.
    pub(super) fn of(rules: &TradingRules, depth: u16) -> Self {
        let price = &rules.common.price;
        Self {
            precision: ListingPrecision::of(rules),
            quote_asset: price.quote_asset.clone(),
            quantity_unit: rules.common.quantity.unit.clone(),
            depth,
        }
    }
}

/// One published order book: both sides in best-first order, with the sequence
/// the venue had reached when it was taken.
#[derive(Clone, Debug)]
pub(super) struct OrderBookSnapshot {
    pub asks: Arc<[ListingOrderBookLevel]>,
    pub bids: Arc<[ListingOrderBookLevel]>,
    pub sequence: MarketSequence,
    pub metadata: OrderBookMetadata,
}

impl OrderBookSnapshot {
    /// Takes a snapshot of a listing's book, keeping at most `maximum_levels`
    /// levels per side.
    ///
    /// The depth is clamped rather than trusted: it decides how much of a venue's
    /// book is copied on every publish, and a caller asking for more than the
    /// model can hold would only make that copy larger.
    pub(super) fn from_market_participant(
        book: &ListingOrderBook,
        metadata: OrderBookMetadata,
        maximum_levels: usize,
    ) -> Self {
        let maximum_levels = maximum_levels.clamp(1, MAX_LEVELS_PER_SIDE);
        Self {
            asks: book.asks.iter().take(maximum_levels).cloned().collect(),
            bids: book.bids.iter().take(maximum_levels).cloned().collect(),
            sequence: book.sequence.clone(),
            metadata,
        }
    }
}

/// One visible level of a side, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GroupedLevel {
    pub price: Price,
    pub quantity: Quantity,
    pub cumulative_quantity: Quantity,
    pub price_text: SharedString,
    pub quantity_text: SharedString,
    pub cumulative_quantity_text: SharedString,
    /// The depth-bar ratio this level was last painted with, and the ratio it is
    /// heading for. A frame interpolates between them; the model only decides
    /// where the movement starts from.
    pub previous_ratio: f32,
    pub current_ratio: f32,
}

/// One side of a book, grouped to a price step and limited to the visible rows.
#[derive(Debug)]
pub(super) struct GroupedBook {
    /// Asks in ascending price order, bids in descending order: both start at the
    /// best price, which is the level nearest the mid price on screen.
    pub asks: Arc<[GroupedLevel]>,
    pub bids: Arc<[GroupedLevel]>,
    pub price_tick: Tick,
    /// The asset the prices are quoted in, which is what the price column is
    /// labelled with.
    pub quote_asset: AssetId,
    /// What the quantities count, which is what the quantity columns are labelled
    /// with.
    pub quantity_unit: QuantityUnit,
    pub mid_price: Option<Price>,
    pub spread_text: Option<SharedString>,
    pub mid_price_text: Option<SharedString>,
    pub ask_imbalance_text: SharedString,
    pub bid_imbalance_text: SharedString,
    /// The ask share of the visible quantity when this book was last painted, and
    /// the share it is heading for.
    pub previous_imbalance: f32,
    pub current_imbalance: f32,
    group_tick: Tick,
    quantity_tick: Tick,
    spread: Option<Price>,
}

/// The levels one side is grouped from, and the step to group them by.
struct GroupedBookInput<'a> {
    asks: &'a [ListingOrderBookLevel],
    bids: &'a [ListingOrderBookLevel],
    price_tick: Tick,
    quantity_tick: Tick,
    quote_asset: AssetId,
    quantity_unit: QuantityUnit,
    price_multiplier: u32,
    window_size: usize,
}

impl GroupedBook {
    /// Groups a snapshot to `price_multiplier` steps, keeping `window_size` rows
    /// per side.
    ///
    /// `previous` is the book that was drawn last and `previous_animation_progress`
    /// how far its easing had got, which is what makes an unfinished movement
    /// continue from the ratio on screen instead of jumping back to the value it
    /// started from.
    pub(super) fn build(
        snapshot: &OrderBookSnapshot,
        previous: Option<&Self>,
        previous_animation_progress: f32,
        price_multiplier: u32,
        window_size: usize,
    ) -> Self {
        Self::build_from_levels(
            GroupedBookInput {
                asks: &snapshot.asks,
                bids: &snapshot.bids,
                price_tick: snapshot.metadata.precision.price_tick,
                quantity_tick: snapshot.metadata.precision.quantity_tick,
                quote_asset: snapshot.metadata.quote_asset.clone(),
                quantity_unit: snapshot.metadata.quantity_unit.clone(),
                price_multiplier,
                window_size,
            },
            previous,
            previous_animation_progress,
        )
    }

    fn build_from_levels(
        input: GroupedBookInput<'_>,
        previous: Option<&Self>,
        previous_animation_progress: f32,
    ) -> Self {
        let previous_animation_progress = sanitize_ratio(previous_animation_progress);
        let group_tick = multiplied_tick(input.price_tick, input.price_multiplier);
        // Only a book grouped by the same steps can hand its ratios over: the
        // indices mean nothing once the rows are cut differently.
        let previous_levels_match = previous.is_some_and(|book| {
            book.group_tick == group_tick && book.quantity_tick == input.quantity_tick
        });
        let previous_asks = previous
            .filter(|_| previous_levels_match)
            .map_or(&[][..], |book| book.asks.as_ref());
        let previous_bids = previous
            .filter(|_| previous_levels_match)
            .map_or(&[][..], |book| book.bids.as_ref());

        let asks = group_side(
            input.asks,
            Side::Ask,
            group_tick,
            input.quantity_tick,
            input.window_size,
            previous_asks,
            previous_animation_progress,
        );
        let bids = group_side(
            input.bids,
            Side::Bid,
            group_tick,
            input.quantity_tick,
            input.window_size,
            previous_bids,
            previous_animation_progress,
        );
        let best_ask = asks.best_price;
        let best_bid = bids.best_price;
        let spread = best_ask
            .zip(best_bid)
            .and_then(|(ask, bid)| ask.checked_sub(bid));
        let mid_price = best_ask.zip(best_bid).map(|(ask, bid)| midpoint(ask, bid));
        let previous_imbalance = previous.map_or(0.0, |book| {
            interpolated_ratio(
                book.previous_imbalance,
                book.current_imbalance,
                previous_animation_progress,
            )
        });
        let current_imbalance = imbalance(asks.visible_total, bids.visible_total);
        let spread_text = previous
            .filter(|book| book.price_tick == input.price_tick && book.spread == spread)
            .and_then(|book| book.spread_text.clone())
            .or_else(|| {
                spread.map(|price| format_shared(|| price.format_and_round_tick(input.price_tick)))
            });
        let mid_price_text = previous
            .filter(|book| book.price_tick == input.price_tick && book.mid_price == mid_price)
            .and_then(|book| book.mid_price_text.clone())
            .or_else(|| {
                mid_price
                    .map(|price| format_shared(|| price.format_and_round_tick(input.price_tick)))
            });
        let ask_imbalance_text = previous
            .filter(|book| book.current_imbalance == current_imbalance)
            .map(|book| book.ask_imbalance_text.clone())
            .unwrap_or_else(|| format_shared(|| format_smolstr!("{current_imbalance:.2}")));
        let bid_imbalance_text = previous
            .filter(|book| book.current_imbalance == current_imbalance)
            .map(|book| book.bid_imbalance_text.clone())
            .unwrap_or_else(|| format_shared(|| format_smolstr!("{:.2}", 1.0 - current_imbalance)));

        Self {
            asks: asks.levels.into(),
            bids: bids.levels.into(),
            price_tick: input.price_tick,
            quote_asset: input.quote_asset,
            quantity_unit: input.quantity_unit,
            mid_price,
            spread_text,
            mid_price_text,
            ask_imbalance_text,
            bid_imbalance_text,
            previous_imbalance,
            current_imbalance,
            group_tick,
            quantity_tick: input.quantity_tick,
            spread,
        }
    }

    /// Whether this book draws exactly what `other` draws.
    ///
    /// A venue publishes far more often than a book changes on screen, and a
    /// frame that would draw the same pixels is not worth scheduling: this is what
    /// lets the widget skip the redraw instead of publishing a new book to itself.
    pub(super) fn has_same_current_state(&self, other: &Self) -> bool {
        self.price_tick == other.price_tick
            && self.group_tick == other.group_tick
            && self.quantity_tick == other.quantity_tick
            && self.quote_asset == other.quote_asset
            && self.quantity_unit == other.quantity_unit
            && self.mid_price == other.mid_price
            && self.spread == other.spread
            && self.current_imbalance == other.current_imbalance
            && current_levels_equal(&self.asks, &other.asks)
            && current_levels_equal(&self.bids, &other.bids)
    }

    /// The largest movement this book asks a frame to ease, which is what decides
    /// whether easing it is visible at all.
    pub(super) fn animation_span(&self) -> f32 {
        self.asks
            .iter()
            .chain(self.bids.iter())
            .map(|level| (level.current_ratio - level.previous_ratio).abs())
            .fold(
                (self.current_imbalance - self.previous_imbalance).abs(),
                f32::max,
            )
    }
}

/// Which side of the book is being grouped. Asks round up and bids round down,
/// so a bucket never claims a price the venue did not quote on that side.
#[derive(Clone, Copy)]
enum Side {
    Ask,
    Bid,
}

struct GroupedSide {
    levels: Vec<GroupedLevel>,
    visible_total: Quantity,
    best_price: Option<Price>,
}

/// The context one side's levels are built with, shared by every row.
struct GroupedLevelContext {
    visible_total: Quantity,
    previous_animation_progress: f32,
    group_tick: Tick,
    quantity_tick: Tick,
}

/// Groups one side's levels into price buckets, best first.
///
/// The window is applied while bucketing rather than afterwards: a book has
/// thousands of levels and only the best few dozen are ever drawn, so the far end
/// is dropped as it is walked instead of being collected and then cut.
fn group_side(
    source_levels: &[ListingOrderBookLevel],
    side: Side,
    group_tick: Tick,
    quantity_tick: Tick,
    window_size: usize,
    previous: &[GroupedLevel],
    previous_animation_progress: f32,
) -> GroupedSide {
    let step = group_tick.inner();
    let mut buckets = BTreeMap::<Price, Quantity>::new();
    let mut best_price = None;
    for level in source_levels {
        let price = level.price.value;
        let quantity = level.quantity.value;
        if quantity.is_zero() {
            continue;
        }
        best_price = Some(best_price.map_or(price, |best: Price| match side {
            Side::Ask => best.min(price),
            Side::Bid => best.max(price),
        }));
        let price = Price::from_inner(group_price(price.inner(), step, side));
        if let Some(bucketed) = buckets.get_mut(&price) {
            *bucketed = Quantity::from_inner(bucketed.inner().saturating_add(quantity.inner()));
            continue;
        }

        let can_enter_window = buckets.len() < window_size
            || match side {
                Side::Ask => buckets
                    .last_key_value()
                    .is_some_and(|(&worst, _)| price < worst),
                Side::Bid => buckets
                    .first_key_value()
                    .is_some_and(|(&worst, _)| price > worst),
            };
        if !can_enter_window {
            continue;
        }

        if buckets.len() == window_size {
            match side {
                Side::Ask => {
                    buckets.pop_last();
                }
                Side::Bid => {
                    buckets.pop_first();
                }
            }
        }
        buckets.insert(price, quantity);
    }

    let visible_total = Quantity::from_inner(buckets.values().fold(0_u128, |total, quantity| {
        total.saturating_add(quantity.inner())
    }));
    let mut cumulative = Quantity::ZERO;
    let mut levels = Vec::with_capacity(buckets.len());
    let level_context = GroupedLevelContext {
        visible_total,
        previous_animation_progress,
        group_tick,
        quantity_tick,
    };
    // Asks are grouped in ascending order and bids in descending order, so both
    // start at the best price and walk away from the mid price.
    match side {
        Side::Ask => {
            for (index, (price, quantity)) in buckets.into_iter().enumerate() {
                push_grouped_level(
                    &mut levels,
                    &mut cumulative,
                    price,
                    quantity,
                    previous.get(index),
                    &level_context,
                );
            }
        }
        Side::Bid => {
            for (index, (price, quantity)) in buckets.into_iter().rev().enumerate() {
                push_grouped_level(
                    &mut levels,
                    &mut cumulative,
                    price,
                    quantity,
                    previous.get(index),
                    &level_context,
                );
            }
        }
    }

    GroupedSide {
        levels,
        visible_total,
        best_price,
    }
}

/// Adds one row, cumulating the side's quantity as it goes.
///
/// Each label is reused from the row that was drawn at the same index when it
/// still shows the same value, which is what keeps a book that only moved at the
/// top of the ladder from reformatting every row below it.
fn push_grouped_level(
    levels: &mut Vec<GroupedLevel>,
    cumulative: &mut Quantity,
    price: Price,
    quantity: Quantity,
    previous: Option<&GroupedLevel>,
    context: &GroupedLevelContext,
) {
    *cumulative = Quantity::from_inner(cumulative.inner().saturating_add(quantity.inner()));
    let price_text = previous
        .filter(|level| level.price == price)
        .map(|level| level.price_text.clone())
        .unwrap_or_else(|| format_shared(|| price.format_and_round_tick(context.group_tick)));
    let quantity_text = previous
        .filter(|level| level.quantity == quantity)
        .map(|level| level.quantity_text.clone())
        .unwrap_or_else(|| format_shared(|| quantity.format_and_round_tick(context.quantity_tick)));
    let cumulative_quantity_text = previous
        .filter(|level| level.cumulative_quantity == *cumulative)
        .map(|level| level.cumulative_quantity_text.clone())
        .unwrap_or_else(|| {
            format_shared(|| cumulative.format_and_round_tick(context.quantity_tick))
        });
    levels.push(GroupedLevel {
        price,
        quantity,
        cumulative_quantity: *cumulative,
        price_text,
        quantity_text,
        cumulative_quantity_text,
        previous_ratio: previous.map_or(0.0, |level| {
            interpolated_ratio(
                level.previous_ratio,
                level.current_ratio,
                context.previous_animation_progress,
            )
        }),
        current_ratio: ratio(cumulative.inner(), context.visible_total.inner()),
    });
}

fn current_levels_equal(left: &[GroupedLevel], right: &[GroupedLevel]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.price == right.price
                && left.quantity == right.quantity
                && left.cumulative_quantity == right.cumulative_quantity
                && left.current_ratio == right.current_ratio
        })
}

fn midpoint(left: Price, right: Price) -> Price {
    let low = left.min(right).inner();
    let high = left.max(right).inner();
    Price::from_inner(low.saturating_add(high.saturating_sub(low) / 2))
}

/// The ask share of the visible quantity, which is what the imbalance bar splits
/// on. A book with nothing visible on either side is neutral rather than empty.
fn imbalance(ask_total: Quantity, bid_total: Quantity) -> f32 {
    let ask = ask_total.inner() as f64;
    let bid = bid_total.inner() as f64;
    let total = ask + bid;
    if total == 0.0 {
        0.5
    } else {
        sanitize_ratio((ask / total) as f32)
    }
}

fn ratio(value: u128, total: u128) -> f32 {
    if total == 0 {
        0.0
    } else {
        sanitize_ratio((value as f64 / total as f64) as f32)
    }
}

pub(super) fn interpolated_ratio(previous: f32, current: f32, progress: f32) -> f32 {
    (previous + (current - previous) * progress).clamp(0.0, 1.0)
}

/// Whether `sequence` is strictly after `other`.
///
/// Two sequences of different representations are not comparable, and an ordering
/// that cannot be stated is never one this book acts on: the answer is `false`, so
/// every caller falls back to comparing only what both ends agree on — a whole
/// snapshot.
pub(super) fn is_after(sequence: &MarketSequence, other: &MarketSequence) -> bool {
    sequence.is_after(other) == Some(true)
}

fn sanitize_ratio(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn floor_to_step(value: u128, step: u128) -> u128 {
    value
        .checked_div(step)
        .map_or(value, |groups| groups.saturating_mul(step))
}

fn ceil_to_step(value: u128, step: u128) -> u128 {
    let Some(remainder) = value.checked_rem(step) else {
        return value;
    };
    if remainder == 0 {
        value
    } else {
        value.saturating_add(step - remainder)
    }
}

fn group_price(value: u128, step: u128, side: Side) -> u128 {
    match side {
        Side::Ask => ceil_to_step(value, step),
        Side::Bid => floor_to_step(value, step),
    }
}

/// The step one bucket spans: the venue's price tick multiplied by the user's
/// grouping choice. A product that no longer fits a tick keeps the venue's own
/// step rather than inventing one.
fn multiplied_tick(price_tick: Tick, multiplier: u32) -> Tick {
    let inner = price_tick.inner().saturating_mul(u128::from(multiplier));
    Tick::from_inner(inner).unwrap_or(price_tick)
}

/// The label of one price-grouping choice: the step it groups to, written the way
/// the venue writes prices.
pub(super) fn price_step_label(price_tick: Option<Tick>, multiplier: u32) -> SharedString {
    let Some(price_tick) = price_tick else {
        return SharedString::new_static("--");
    };
    let Some(inner) = price_tick.inner().checked_mul(u128::from(multiplier)) else {
        return format_shared(|| format_smolstr!("{multiplier}x"));
    };
    let Ok(group_tick) = Tick::from_inner(inner) else {
        return format_shared(|| format_smolstr!("{multiplier}x"));
    };
    format_shared(|| Price::from_inner(inner).format_and_round_tick(group_tick))
}

/// Every price-grouping label, in the order the choices are offered.
pub(super) fn price_step_labels(
    price_tick: Option<Tick>,
) -> [SharedString; PRICE_MULTIPLIERS.len()] {
    PRICE_MULTIPLIERS.map(|multiplier| price_step_label(price_tick, multiplier))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use market_participants_model::{
        asset::{AssetId, CurrencyId, Iso4217Code},
        listing::{route::VenueListingKey, venue::PriceBasis},
        market::{price::MarketPrice, units::MarketQuantity},
    };

    use super::*;

    fn asset(code: &str) -> AssetId {
        let code = Iso4217Code::new(code).expect("the test currency code should be valid");
        AssetId::Currency(CurrencyId::new(code))
    }

    fn price(value: &str) -> Price {
        Price::from_str(value).expect("price should parse")
    }

    fn quantity(value: &str) -> Quantity {
        Quantity::from_str(value).expect("quantity should parse")
    }

    fn tick(value: &str) -> Tick {
        Tick::from_str(value).expect("tick should parse")
    }

    fn metadata() -> OrderBookMetadata {
        OrderBookMetadata {
            precision: ListingPrecision {
                price_tick: tick("1"),
                quantity_tick: tick("1"),
            },
            quote_asset: asset("USD"),
            quantity_unit: QuantityUnit::AssetUnits {
                asset: asset("USD"),
            },
            depth: 5_000,
        }
    }

    fn level(price_value: &str, quantity_value: &str) -> ListingOrderBookLevel {
        ListingOrderBookLevel {
            price: MarketPrice {
                value: price(price_value),
                quote_asset: asset("USD"),
                basis: PriceBasis::PerAssetUnit,
            },
            quantity: MarketQuantity {
                value: quantity(quantity_value),
                unit: QuantityUnit::AssetUnits {
                    asset: asset("USD"),
                },
            },
        }
    }

    fn build(
        asks: &[ListingOrderBookLevel],
        bids: &[ListingOrderBookLevel],
        previous: Option<&GroupedBook>,
        multiplier: u32,
        window_size: usize,
    ) -> GroupedBook {
        build_at_progress(asks, bids, previous, 1.0, multiplier, window_size)
    }

    fn build_at_progress(
        asks: &[ListingOrderBookLevel],
        bids: &[ListingOrderBookLevel],
        previous: Option<&GroupedBook>,
        previous_animation_progress: f32,
        multiplier: u32,
        window_size: usize,
    ) -> GroupedBook {
        let metadata = metadata();
        GroupedBook::build_from_levels(
            GroupedBookInput {
                asks,
                bids,
                price_tick: metadata.precision.price_tick,
                quantity_tick: metadata.precision.quantity_tick,
                quote_asset: metadata.quote_asset,
                quantity_unit: metadata.quantity_unit,
                price_multiplier: multiplier,
                window_size,
            },
            previous,
            previous_animation_progress,
        )
    }

    #[test]
    fn snapshot_should_preserve_network_order_and_sequence() {
        let book = ListingOrderBook {
            listing: listing_key(),
            sequence: MarketSequence::Numeric(42),
            timestamp: None,
            asks: vec![level("101", "1"), level("102", "2")],
            bids: vec![level("100", "1"), level("99", "2")],
        };
        let snapshot = OrderBookSnapshot::from_market_participant(&book, metadata(), 5);

        assert_eq!(
            (
                snapshot.asks[0].price.value,
                snapshot.bids[0].price.value,
                snapshot.sequence.clone(),
            ),
            (price("101"), price("100"), MarketSequence::Numeric(42))
        );
    }

    #[test]
    fn snapshot_should_cap_each_side_at_the_requested_depth() {
        let book = ListingOrderBook {
            listing: listing_key(),
            sequence: MarketSequence::Numeric(1),
            timestamp: None,
            asks: vec![level("101", "1"), level("102", "2"), level("103", "3")],
            bids: vec![level("100", "1"), level("99", "2"), level("98", "3")],
        };
        let snapshot = OrderBookSnapshot::from_market_participant(&book, metadata(), 2);

        assert_eq!((snapshot.asks.len(), snapshot.bids.len()), (2, 2));
    }

    /// The listing key the model's own fixtures publish under. The model never
    /// reads it, but a book is always the book of one listing.
    fn listing_key() -> VenueListingKey {
        market_participants_connector::catalog::test_support::listing_key(
            &market_participants_connector::catalog::test_support::spot_route(),
            "BTCUSDT",
        )
    }

    #[test]
    fn metadata_should_come_from_the_listings_own_rules() {
        let quote = asset("USD");
        let base = asset("EUR");

        let metadata = OrderBookMetadata::of(
            &market_participants_connector::catalog::test_support::rules(
                quote.clone(),
                base.clone(),
            ),
            5_000,
        );

        assert_eq!(
            (
                metadata.precision.price_tick,
                metadata.precision.quantity_tick,
                metadata.quote_asset,
                metadata.quantity_unit,
                metadata.depth,
            ),
            (
                tick("1"),
                tick("1"),
                quote,
                QuantityUnit::AssetUnits { asset: base },
                5_000,
            )
        );
    }

    #[test]
    fn a_rule_set_without_a_quantity_step_should_round_quantities_to_whole_units() {
        let mut rules =
            market_participants_connector::catalog::test_support::rules(asset("USD"), asset("EUR"));
        rules.common.quantity.step = Quantity::ZERO;

        let metadata = OrderBookMetadata::of(&rules, 5_000);

        assert_eq!(metadata.precision.quantity_tick, tick("1"));
    }

    #[test]
    fn a_shallow_route_should_offer_the_row_limits_its_book_can_fill() {
        assert_eq!(window_sizes(Some(20)), &SHALLOW_WINDOW_SIZES[..]);
        assert_eq!(default_window_size(Some(20)), SHALLOW_WINDOW_SIZES[2]);
    }

    #[test]
    fn a_deep_route_should_offer_the_standard_row_limits() {
        assert_eq!(window_sizes(Some(5_000)), &WINDOW_SIZES[..]);
        assert_eq!(default_window_size(Some(5_000)), WINDOW_SIZES[0]);
        assert_eq!(default_window_size(Some(25)), WINDOW_SIZES[0]);
    }

    #[test]
    fn a_route_whose_depth_is_not_known_should_keep_the_standard_row_limits() {
        assert_eq!(window_sizes(None), &WINDOW_SIZES[..]);
        assert_eq!(default_window_size(None), WINDOW_SIZES[0]);
    }

    #[test]
    fn grouping_should_complete_each_bucket_without_leaking_the_next_bucket() {
        let asks = [
            level("101", "1"),
            level("109", "2"),
            level("110", "4"),
            level("119", "5"),
        ];

        let book = build(&asks, &[], None, 10, 25);

        assert_eq!(book.asks.len(), 2);
        assert_eq!(book.asks[0].quantity_text, "7");
        assert_eq!(book.asks[0].cumulative_quantity_text, "7");
        assert_eq!(book.asks[1].quantity_text, "5");
        assert_eq!(book.asks[1].cumulative_quantity_text, "12");
    }

    #[test]
    fn grouping_should_emit_the_final_bucket() {
        let asks = [level("101", "1"), level("111", "2"), level("121", "3")];

        let book = build(&asks, &[], None, 10, 25);

        assert_eq!(
            book.asks
                .iter()
                .map(|level| level.price)
                .collect::<Vec<_>>(),
            [price("110"), price("120"), price("130")]
        );
        assert_eq!(book.asks[2].quantity_text, "3");
        assert_eq!(book.asks[2].cumulative_quantity_text, "6");
    }

    #[test]
    fn grouped_prices_should_round_asks_up_and_bids_down() {
        let asks = [level("101", "1"), level("110", "2"), level("111", "3")];
        let bids = [level("119", "1"), level("110", "2"), level("109", "3")];

        let book = build(&asks, &bids, None, 10, 25);
        let asks = book
            .asks
            .iter()
            .map(|level| (level.price, level.quantity_text.clone()))
            .collect::<Vec<_>>();
        let bids = book
            .bids
            .iter()
            .map(|level| (level.price, level.quantity_text.clone()))
            .collect::<Vec<_>>();

        assert_eq!(
            (asks, bids),
            (
                vec![(price("110"), "3".into()), (price("120"), "3".into())],
                vec![(price("110"), "3".into()), (price("100"), "3".into())],
            )
        );
    }

    #[test]
    fn ask_rounding_should_preserve_boundaries_and_saturate_on_overflow() {
        assert_eq!(
            (
                ceil_to_step(110, 10),
                ceil_to_step(u128::MAX, 10),
                ceil_to_step(7, 0),
            ),
            (110, u128::MAX, 7)
        );
    }

    #[test]
    fn sides_should_use_best_first_order_and_apply_the_window() {
        let asks = (1..=30)
            .map(|value| level(&value.to_string(), "1"))
            .collect::<Vec<_>>();
        let bids = asks.clone();

        let book = build(&asks, &bids, None, 1, 25);

        assert_eq!(book.asks.len(), 25);
        assert_eq!(book.bids.len(), 25);
        assert_eq!(book.asks.first().map(|level| level.price), Some(price("1")));
        assert_eq!(book.asks.last().map(|level| level.price), Some(price("25")));
        assert_eq!(
            book.bids.first().map(|level| level.price),
            Some(price("30"))
        );
        assert_eq!(book.bids.last().map(|level| level.price), Some(price("6")));
    }

    #[test]
    fn grouping_should_accept_a_layout_limited_window_size() {
        let asks = (1..=10)
            .map(|value| level(&value.to_string(), "1"))
            .collect::<Vec<_>>();

        let book = build(&asks, &[], None, 1, 3);

        assert_eq!(book.asks.len(), 3);
        assert_eq!(book.asks.last().map(|level| level.price), Some(price("3")));
    }

    #[test]
    fn inserted_level_should_inherit_the_previous_visible_row_ratios() {
        let first = build(
            &[
                level("100", "1"),
                level("110", "2"),
                level("120", "3"),
                level("130", "4"),
            ],
            &[],
            None,
            10,
            3,
        );
        let second = build(
            &[
                level("90", "4"),
                level("100", "1"),
                level("110", "2"),
                level("120", "3"),
            ],
            &[],
            Some(&first),
            10,
            3,
        );

        assert_eq!(second.asks[0].price, price("90"));
        assert!((second.asks[0].previous_ratio - 1.0 / 6.0).abs() < f32::EPSILON);
        assert_eq!(second.asks[1].price, price("100"));
        assert!((second.asks[1].previous_ratio - 0.5).abs() < f32::EPSILON);
        assert_eq!(second.asks[2].price, price("110"));
        assert!((second.asks[2].previous_ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn removed_level_should_be_replaced_without_an_empty_animation_row() {
        let first = build(
            &[
                level("100", "1"),
                level("110", "2"),
                level("120", "3"),
                level("130", "4"),
            ],
            &[],
            None,
            10,
            3,
        );
        let second = build(
            &[level("100", "1"), level("120", "3"), level("130", "4")],
            &[],
            Some(&first),
            10,
            3,
        );

        assert_eq!(
            second
                .asks
                .iter()
                .map(|level| level.price)
                .collect::<Vec<_>>(),
            [price("100"), price("120"), price("130")]
        );
        assert!((second.asks[0].previous_ratio - 1.0 / 6.0).abs() < f32::EPSILON);
        assert!((second.asks[1].previous_ratio - 0.5).abs() < f32::EPSILON);
        assert!((second.asks[2].previous_ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn removed_bid_level_should_be_replaced_in_descending_order() {
        let first = build(
            &[],
            &[
                level("100", "1"),
                level("90", "2"),
                level("80", "3"),
                level("70", "4"),
            ],
            None,
            10,
            3,
        );
        let second = build(
            &[],
            &[level("100", "1"), level("80", "3"), level("70", "4")],
            Some(&first),
            10,
            3,
        );

        assert_eq!(
            second
                .bids
                .iter()
                .map(|level| level.price)
                .collect::<Vec<_>>(),
            [price("100"), price("80"), price("70")]
        );
        assert!((second.bids[0].previous_ratio - 1.0 / 6.0).abs() < f32::EPSILON);
        assert!((second.bids[1].previous_ratio - 0.5).abs() < f32::EPSILON);
        assert!((second.bids[2].previous_ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn ratios_and_imbalance_should_use_visible_totals() {
        let first = build(
            &[level("101", "1"), level("102", "3")],
            &[level("99", "4")],
            None,
            1,
            25,
        );
        let second = build(
            &[level("101", "2"), level("102", "2")],
            &[level("99", "12")],
            Some(&first),
            1,
            25,
        );

        assert!((first.asks[0].current_ratio - 0.25).abs() < f32::EPSILON);
        assert!((first.current_imbalance - 0.5).abs() < f32::EPSILON);
        assert!((second.previous_imbalance - 0.5).abs() < f32::EPSILON);
        assert!((second.current_imbalance - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn unfinished_animation_should_retarget_from_the_painted_state() {
        let first = build(
            &[level("101", "1"), level("102", "3")],
            &[level("99", "4")],
            None,
            1,
            25,
        );
        let second = build_at_progress(
            &[level("101", "2"), level("102", "2")],
            &[level("99", "12")],
            Some(&first),
            0.4,
            1,
            25,
        );

        assert!((second.asks[0].previous_ratio - 0.1).abs() < f32::EPSILON);
        assert!((second.asks[1].previous_ratio - 0.4).abs() < f32::EPSILON);
        assert!((second.previous_imbalance - 0.2).abs() < f32::EPSILON);
        assert!((second.animation_span() - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn unchanged_visible_levels_should_reuse_formatted_text() {
        FORMAT_COUNT.with(|count| count.set(0));
        let asks = [ListingOrderBookLevel {
            price: MarketPrice {
                value: Price::from_inner(u128::MAX),
                quote_asset: asset("USD"),
                basis: PriceBasis::PerAssetUnit,
            },
            quantity: MarketQuantity {
                value: Quantity::from_inner(u128::MAX),
                unit: QuantityUnit::AssetUnits {
                    asset: asset("USD"),
                },
            },
        }];
        let first = build(&asks, &[], None, 1, 25);
        let first_format_count = FORMAT_COUNT.with(std::cell::Cell::get);

        let _second = build(&asks, &[], Some(&first), 1, 25);

        assert_eq!(FORMAT_COUNT.with(std::cell::Cell::get), first_format_count);
    }

    #[test]
    fn regrouping_after_a_grouping_change_should_format_every_label_again() {
        FORMAT_COUNT.with(|count| count.set(0));
        let asks = [level("101", "1")];
        let first = build(&asks, &[], None, 1, 25);
        let first_format_count = FORMAT_COUNT.with(std::cell::Cell::get);

        let _second = build(&asks, &[], Some(&first), 10, 25);

        assert!(FORMAT_COUNT.with(std::cell::Cell::get) > first_format_count);
    }

    #[test]
    fn far_depth_change_should_not_change_the_visible_grouped_state() {
        let first = build(
            &[
                level("101", "1"),
                level("102", "2"),
                level("103", "3"),
                level("200", "4"),
            ],
            &[level("100", "6")],
            None,
            1,
            3,
        );
        let second = build(
            &[
                level("101", "1"),
                level("102", "2"),
                level("103", "3"),
                level("200", "40"),
            ],
            &[level("100", "6")],
            Some(&first),
            1,
            3,
        );

        assert!(first.has_same_current_state(&second));
    }

    #[test]
    fn empty_side_should_have_no_spread_or_mid_price() {
        let book = build(&[], &[level("99", "4")], None, 1, 25);

        assert!(book.asks.is_empty());
        assert_eq!(book.current_imbalance, 0.0);
        assert_eq!(book.spread_text, None);
        assert_eq!(book.mid_price_text, None);
    }

    #[test]
    fn empty_book_should_render_a_neutral_imbalance() {
        let book = build(&[], &[], None, 1, 25);

        assert_eq!(book.current_imbalance, 0.5);
    }

    #[test]
    fn zero_quantity_levels_should_not_be_grouped() {
        let book = build(
            &[level("101", "0"), level("102", "2")],
            &[level("99", "1")],
            None,
            1,
            25,
        );

        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.asks[0].price, price("102"));
        assert_eq!(book.mid_price, Some(price("100.5")));
    }

    #[test]
    fn quantity_aggregation_should_saturate_without_panicking() {
        let mut widest = level("100", "1");
        widest.quantity.value = Quantity::from_inner(u128::MAX);
        let asks = [widest, level("100", "1")];

        let book = build(&asks, &[], None, 1, 25);

        let expected = Quantity::from_inner(u128::MAX).format_and_round_tick(tick("1"));
        assert_eq!(book.asks[0].quantity_text.as_str(), expected.as_str());
        assert_eq!(book.asks[0].current_ratio, 1.0);
    }

    #[test]
    fn price_step_labels_should_follow_the_market_tick() {
        let labels = price_step_labels(Some(tick("0.01")));

        assert_eq!(
            labels.map(|label| label.to_string()),
            ["0.01", "0.1", "1", "10", "100"]
        );
    }

    #[test]
    fn price_step_labels_should_use_a_placeholder_without_metadata() {
        assert_eq!(price_step_label(None, 1), "--");
    }

    #[test]
    fn price_step_label_should_survive_an_overflowing_multiplier() {
        let widest = Tick::from_inner(u128::MAX).expect("the widest tick should be valid");

        assert_eq!(price_step_label(Some(widest), 2), "2x");
    }
}
