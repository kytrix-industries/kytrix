//! The candle series a candlestick chart draws.
//!
//! A chart is fed from three directions — the initial history page, older pages
//! fetched by panning left, and newer pages fetched by panning right — while the
//! live subscription keeps overwriting the newest candle. All three go through
//! [`CandlestickSeries`], which keeps one `timestamp -> candle` map bounded by a
//! capacity and decides what each merge is allowed to evict:
//!
//! - history arriving forwards and live updates evict the **oldest** candle,
//! - a page fetched by panning left evicts whatever the caller allows: with
//!   [`NewestCandle::Evicted`] the **newest** one, and only while the buffer is
//!   full, which is the one case that evicts anything at all; with
//!   [`NewestCandle::Kept`] nothing, because a page that will not fit is trimmed
//!   instead.
//!
//! Which of the two an older page gets is not this module's decision: whether the
//! newest candle is on screen is a property of the viewport the chart is drawn
//! with, and the series cannot see it. So the caller states the intent with every
//! page it asks for, which is why [`CandlestickSeries::merge_older`] takes one.
//!
//! That asymmetry is what keeps a historical window stable: the user is looking
//! at a fixed range, and a live tick must not push it out from under them. It is
//! also why a page merged with the newest candle kept leaves the chart exactly
//! where it was: nothing is evicted, so nothing is moved, and
//! [`CandlestickSeries::at_live_edge`] stays true.
//!
//! [`CandlestickSeries::snapshot`] hands the chart an immutable
//! [`CandlestickSeriesSnapshot`]. The candle data is shared rather than copied,
//! and [`CandlestickSeriesSnapshot::shares_chart_data_with`] lets the painter
//! decide in constant time whether the previous drawing is still valid.
//!
//! The candles are the listing-scoped values the market facade published, so
//! every number on the chart carries its unit. What the chart draws is the
//! number: [`CandlePrices`] is the one place a candle's price legs are resolved,
//! which keeps the quote asset and the price basis out of every drawing site.

use std::{collections::BTreeMap, sync::Arc};

use market_participants_model::{
    decimal::{Price, Tick},
    listing::rules::ListingPrecision,
    market::book::ListingCandlestick,
};

/// The four prices of one candle, as the chart reads them.
///
/// The legs are private because resolving them is what this type is for: every
/// drawing site reads a price through [`CandlePrices::of`], so the quote asset
/// and the price basis a candle also carries never reach one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CandlePrices {
    open: Price,
    high: Price,
    low: Price,
    close: Price,
}

impl CandlePrices {
    /// Resolves the price legs of one candle.
    pub(super) fn of(candle: &ListingCandlestick) -> Self {
        Self {
            open: candle.open.value,
            high: candle.high.value,
            low: candle.low.value,
            close: candle.close.value,
        }
    }

    pub(super) const fn open(self) -> Price {
        self.open
    }

    pub(super) const fn high(self) -> Price {
        self.high
    }

    pub(super) const fn low(self) -> Price {
        self.low
    }

    pub(super) const fn close(self) -> Price {
        self.close
    }

    /// Whether the candle closed at or above its open.
    pub(super) const fn is_up(self) -> bool {
        self.close.inner() >= self.open.inner()
    }
}

/// One accepted series, as the view and the chart painter see it.
///
/// The candle data is behind an [`Arc`], so handing the snapshot to the stream,
/// to the view and to the painter costs a refcount rather than a copy of up to
/// `MAXIMUM_CANDLES` candles.
#[derive(Clone, Debug)]
pub(super) struct CandlestickSeriesSnapshot {
    candles: Arc<[ListingCandlestick]>,
    precision: ListingPrecision,
    older_exhausted: bool,
    at_live_edge: bool,
}

impl CandlestickSeriesSnapshot {
    pub(super) fn candles(&self) -> &[ListingCandlestick] {
        &self.candles
    }

    pub(super) fn earliest_timestamp(&self) -> Option<i64> {
        self.candles.first().map(|candle| candle.timestamp)
    }

    pub(super) fn latest_timestamp(&self) -> Option<i64> {
        self.candles.last().map(|candle| candle.timestamp)
    }

    pub(super) fn latest(&self) -> Option<&ListingCandlestick> {
        self.candles.last()
    }

    /// The price increment every label on the chart is rounded to.
    pub(super) const fn price_tick(&self) -> Tick {
        self.precision.price_tick
    }

    /// Whether the venue has no more history before the earliest candle.
    pub(super) const fn older_exhausted(&self) -> bool {
        self.older_exhausted
    }

    /// Whether the newest candle is the one the venue is still building.
    pub(super) const fn at_live_edge(&self) -> bool {
        self.at_live_edge
    }

    /// Whether two snapshots would draw the same candles.
    ///
    /// Deliberately ignores [`older_exhausted`], which says what more can be
    /// fetched rather than what is drawn; the painter must not rebuild a chart
    /// because pagination ran out.
    ///
    /// [`older_exhausted`]: CandlestickSeriesSnapshot::older_exhausted
    pub(super) fn shares_chart_data_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.candles, &other.candles)
            && self.precision == other.precision
            && self.at_live_edge == other.at_live_edge
    }
}

/// What an older page may do to the newest candle the series holds.
///
/// The question belongs to the viewport rather than to the series: whether the
/// newest candle is on screen is something only the chart that is drawn can
/// answer, and the series has no way to see it. So the caller states its intent
/// with the request, and the series applies it when the page lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NewestCandle {
    /// The chart follows the newest candle, so it is on screen and stays.
    ///
    /// A page that does not fit is trimmed to the room left, keeping the candles
    /// nearest the buffer: they are the ones that join it without a gap.
    Kept,
    /// The chart is anchored to a window further back, so the newest candle is
    /// off screen to the right. Evicting it is what lets the series move back in
    /// time, and it is also what takes the series off the live edge.
    Evicted,
}

/// What merging one older page did to the series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OlderMerge {
    /// The page added candles before the earliest one held, so the window moved
    /// back in time.
    Advanced,
    /// The page was not empty and none of it fitted, because the buffer was
    /// full. The buffer is what refused it, so this is not an answer about the
    /// venue's history.
    TrimmedByCapacity,
    /// The page changed nothing: it was empty, or every candle it carried is one
    /// the series already held. For a fetched, non-empty page this is the venue
    /// saying there is nothing older.
    MadeNoProgress,
}

/// The mutable candle map behind one chart.
pub(super) struct CandlestickSeries {
    entries: BTreeMap<i64, ListingCandlestick>,
    snapshot_candles: Arc<[ListingCandlestick]>,
    snapshot_dirty: bool,
    maximum_len: usize,
    older_exhausted: bool,
    at_live_edge: bool,
    precision: ListingPrecision,
}

impl CandlestickSeries {
    /// Creates an empty series holding at most `maximum_len` candles.
    ///
    /// A zero capacity would make the series unable to hold the candle it is
    /// currently building, so it is clamped here rather than trusted.
    pub(super) fn new(maximum_len: usize, precision: ListingPrecision) -> Self {
        Self {
            entries: BTreeMap::new(),
            snapshot_candles: Arc::from([]),
            snapshot_dirty: false,
            maximum_len: maximum_len.max(1),
            older_exhausted: false,
            at_live_edge: true,
            precision,
        }
    }

    /// Merges the first history page and settles on the live edge.
    pub(super) fn merge_initial(&mut self, candles: impl IntoIterator<Item = ListingCandlestick>) {
        self.insert_keeping_newest(candles);
        self.at_live_edge = true;
    }

    /// Merges a page fetched by panning left, as the caller's intent allows.
    ///
    /// `newest` says whether the candle at the live edge is on screen. With
    /// [`NewestCandle::Evicted`] evicting it is what takes the series off the live
    /// edge: the user asked to look at the past, so the window must stop following
    /// it. With [`NewestCandle::Kept`] it is on screen and stays, so the page is
    /// trimmed to the room left instead.
    pub(super) fn merge_older(
        &mut self,
        candles: impl IntoIterator<Item = ListingCandlestick>,
        newest: NewestCandle,
    ) -> OlderMerge {
        match newest {
            NewestCandle::Kept => self.insert_trimming_to_room(candles),
            NewestCandle::Evicted => self.insert_keeping_oldest(candles),
        }
    }

    /// Merges a page fetched by panning right.
    pub(super) fn merge_newer(&mut self, candles: impl IntoIterator<Item = ListingCandlestick>) {
        self.insert_keeping_newest(candles);
    }

    /// Applies one live candle, returning whether it changed anything.
    ///
    /// A live update is ignored entirely while the chart shows a historical
    /// window: the venue is still building a candle that is off-screen, and
    /// inserting it would move the range the user is reading.
    pub(super) fn upsert_live(&mut self, candle: &ListingCandlestick) -> bool {
        if !self.at_live_edge {
            return false;
        }
        if let Some(current) = self.entries.get_mut(&candle.timestamp) {
            if current == candle {
                return false;
            }
            *current = candle.clone();
            self.snapshot_dirty = true;
            return true;
        }
        if self.entries.len() == self.maximum_len {
            if self
                .entries
                .first_key_value()
                .is_some_and(|(&earliest, _)| candle.timestamp < earliest)
            {
                return false;
            }
            self.entries.pop_first();
            self.older_exhausted = false;
        }
        self.entries.insert(candle.timestamp, candle.clone());
        self.snapshot_dirty = true;
        true
    }

    pub(super) fn earliest_timestamp(&self) -> Option<i64> {
        self.entries
            .first_key_value()
            .map(|(timestamp, _)| *timestamp)
    }

    pub(super) fn latest_timestamp(&self) -> Option<i64> {
        self.entries
            .last_key_value()
            .map(|(timestamp, _)| *timestamp)
    }

    pub(super) fn mark_older_exhausted(&mut self) {
        self.older_exhausted = true;
    }

    pub(super) fn mark_at_live_edge(&mut self) {
        self.at_live_edge = true;
    }

    /// Whether the venue has no more history before the earliest candle.
    pub(super) const fn older_exhausted(&self) -> bool {
        self.older_exhausted
    }

    /// Whether the newest candle is the one the venue is still building.
    pub(super) const fn at_live_edge(&self) -> bool {
        self.at_live_edge
    }

    /// Returns the current series, rebuilding the shared candle buffer only when
    /// the candles themselves changed.
    ///
    /// Metadata-only changes — running out of history, moving back to the live
    /// edge — reuse the previous allocation, so a snapshot that differs only in
    /// flags still compares equal to the previous one by
    /// [`CandlestickSeriesSnapshot::shares_chart_data_with`].
    pub(super) fn snapshot(&mut self) -> CandlestickSeriesSnapshot {
        if self.snapshot_dirty {
            let mut candles = Vec::with_capacity(self.entries.len());
            candles.extend(self.entries.values().cloned());
            self.snapshot_candles = candles.into();
            self.snapshot_dirty = false;
        }
        CandlestickSeriesSnapshot {
            candles: self.snapshot_candles.clone(),
            precision: self.precision,
            older_exhausted: self.older_exhausted,
            at_live_edge: self.at_live_edge,
        }
    }

    fn insert_keeping_newest(&mut self, candles: impl IntoIterator<Item = ListingCandlestick>) {
        for candle in candles {
            if self.replace_existing(&candle) {
                continue;
            }
            if self.entries.len() == self.maximum_len {
                let Some((&earliest, _)) = self.entries.first_key_value() else {
                    continue;
                };
                if candle.timestamp < earliest {
                    continue;
                }
                self.entries.pop_first();
                // Capacity was the only reason history looked exhausted.
                self.older_exhausted = false;
            }
            self.entries.insert(candle.timestamp, candle);
            self.snapshot_dirty = true;
        }
    }

    /// Inserts an older page without ever evicting, and says what it did.
    ///
    /// The page is trimmed to the room left, keeping the candles nearest the
    /// buffer: they are the ones that join it without a gap. Nothing else is
    /// touched — a series at capacity that receives this page stays exactly where
    /// it was, at the live edge it was following.
    fn insert_trimming_to_room(
        &mut self,
        candles: impl IntoIterator<Item = ListingCandlestick>,
    ) -> OlderMerge {
        let page = candles.into_iter().collect::<Vec<_>>();
        if page.is_empty() {
            return OlderMerge::MadeNoProgress;
        }
        let room = self.maximum_len.saturating_sub(self.entries.len());
        if room == 0 {
            // Nothing fits and nothing may be evicted, so the page is dropped
            // whole. The capacity is what refused it, not the venue.
            return OlderMerge::TrimmedByCapacity;
        }
        let mut inserted = false;
        for candle in bounded_history(page, room) {
            if self.replace_existing(&candle) {
                continue;
            }
            self.entries.insert(candle.timestamp, candle);
            self.snapshot_dirty = true;
            inserted = true;
        }
        if inserted {
            OlderMerge::Advanced
        } else {
            OlderMerge::MadeNoProgress
        }
    }

    /// Inserts an older page by evicting the newest candle when the buffer is
    /// full, and reports what the page did.
    fn insert_keeping_oldest(
        &mut self,
        candles: impl IntoIterator<Item = ListingCandlestick>,
    ) -> OlderMerge {
        let mut evicted_newest = false;
        let mut inserted = false;
        for candle in candles {
            if self.replace_existing(&candle) {
                continue;
            }
            if self.entries.len() == self.maximum_len {
                let Some((&latest, _)) = self.entries.last_key_value() else {
                    continue;
                };
                if candle.timestamp > latest {
                    continue;
                }
                self.entries.pop_last();
                evicted_newest = true;
            }
            self.entries.insert(candle.timestamp, candle);
            self.snapshot_dirty = true;
            inserted = true;
        }
        if evicted_newest {
            // Evicting the newest candle is what takes the series off the live
            // edge, not the page: a page that fitted below capacity moved the
            // window back without losing the candle the chart was following.
            self.at_live_edge = false;
        }
        if inserted {
            OlderMerge::Advanced
        } else {
            OlderMerge::MadeNoProgress
        }
    }

    fn replace_existing(&mut self, candle: &ListingCandlestick) -> bool {
        let Some(current) = self.entries.get_mut(&candle.timestamp) else {
            return false;
        };
        if current != candle {
            *current = candle.clone();
            self.snapshot_dirty = true;
        }
        true
    }
}

/// Keeps the newest `limit` rows of a page, deduplicated by timestamp.
///
/// `limit` must be positive. Both callers take it from a capacity that is clamped
/// where that capacity enters the module — [`CandlestickSeries::new`] and
/// `CandlestickService::new` — so a zero here would be a caller bug rather than a
/// buffer that cannot hold anything, and silently keeping one candle would hide it.
///
/// Two callers need the same rule and they must not drift apart: a fetched page
/// is bounded by the buffer it is going into, and an older page merged with the
/// newest candle kept is trimmed to the room left. Both keep the candles nearest
/// the buffer, both let the last value of a repeated timestamp win, and both
/// answer in ascending timestamp order.
pub(super) fn bounded_history(
    history: Vec<ListingCandlestick>,
    limit: usize,
) -> Vec<ListingCandlestick> {
    debug_assert!(limit > 0, "a page keeps at least one candle");
    let mut bounded = BTreeMap::new();
    for candle in history {
        if let Some(current) = bounded.get_mut(&candle.timestamp) {
            *current = candle;
            continue;
        }
        if bounded.len() == limit {
            if bounded
                .first_key_value()
                .is_some_and(|(&earliest, _)| candle.timestamp < earliest)
            {
                continue;
            }
            bounded.pop_first();
        }
        bounded.insert(candle.timestamp, candle);
    }
    bounded.into_values().collect()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        decimal::{Price, Quantity},
        listing::{route::VenueListingKey, venue::PriceBasis},
        market::{
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit},
        },
    };

    use super::*;

    fn listing() -> VenueListingKey {
        test_support::listing_key(&test_support::spot_route(), "BTCUSDT")
    }

    fn market_price(value: Price) -> MarketPrice {
        MarketPrice {
            value,
            quote_asset: test_support::currency("USD"),
            basis: PriceBasis::PerAssetUnit,
        }
    }

    fn candle(timestamp: i64, close: u64) -> ListingCandlestick {
        let price = market_price(Price::from(close));
        ListingCandlestick {
            listing: listing(),
            open: price.clone(),
            high: price.clone(),
            low: price.clone(),
            close: price,
            traded_quantity: Some(MarketQuantity {
                value: Quantity::from(1_u64),
                unit: QuantityUnit::Contracts,
            }),
            traded_notional: None,
            timestamp,
        }
    }

    fn series(maximum_len: usize) -> CandlestickSeries {
        CandlestickSeries::new(maximum_len, precision())
    }

    /// A price tick of one cent and a quantity step of one thousandth.
    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: Tick::from_str("0.01").expect("price tick should parse"),
            quantity_tick: Tick::from_str("0.001").expect("quantity step should parse"),
        }
    }

    fn timestamps(series: &mut CandlestickSeries) -> Vec<i64> {
        series
            .snapshot()
            .candles()
            .iter()
            .map(|candle| candle.timestamp)
            .collect()
    }

    #[test]
    fn merge_initial_should_keep_realtime_value_for_duplicate_timestamp() {
        let mut series = series(10);
        series.merge_initial([candle(1, 10), candle(2, 20)]);
        series.merge_initial([candle(2, 25)]);

        assert_eq!(
            series
                .snapshot()
                .latest()
                .map(|item| CandlePrices::of(item).close()),
            Some(Price::from(25_u64))
        );
    }

    #[test]
    fn merge_older_should_evict_newest_values_over_the_limit() {
        let mut series = series(3);
        series.merge_initial([candle(3, 30), candle(4, 40), candle(5, 50)]);

        let merged = series.merge_older([candle(1, 10), candle(2, 20)], NewestCandle::Evicted);

        assert_eq!(
            (merged, series.snapshot().latest_timestamp()),
            (OlderMerge::Advanced, Some(3))
        );
    }

    #[test]
    fn merge_newer_should_evict_oldest_values_over_the_limit() {
        let mut series = series(3);
        series.merge_initial([candle(1, 10), candle(2, 20), candle(3, 30)]);
        series.merge_older([candle(0, 5)], NewestCandle::Evicted);

        series.merge_newer([candle(4, 40), candle(5, 50)]);

        assert_eq!(series.snapshot().earliest_timestamp(), Some(2));
    }

    #[test]
    fn oversized_initial_batch_should_keep_only_the_newest_values() {
        let mut series = series(3);

        series.merge_initial((1..=100).rev().map(|timestamp| candle(timestamp, 10)));

        assert_eq!(timestamps(&mut series), vec![98, 99, 100]);
    }

    #[test]
    fn oversized_older_batch_should_keep_only_the_oldest_values() {
        let mut series = series(3);
        series.merge_initial([candle(100, 10)]);

        series.merge_older(
            (1..100).rev().map(|timestamp| candle(timestamp, 10)),
            NewestCandle::Evicted,
        );

        assert_eq!(timestamps(&mut series), vec![1, 2, 3]);
    }

    #[test]
    fn oversized_newer_batch_should_keep_only_the_newest_values() {
        let mut series = series(3);
        series.merge_initial([candle(1, 10)]);

        series.merge_newer((2..=100).map(|timestamp| candle(timestamp, 10)));

        assert_eq!(timestamps(&mut series), vec![98, 99, 100]);
    }

    #[test]
    fn reaching_capacity_should_not_mark_older_history_exhausted() {
        let mut series = series(2);
        series.merge_initial([candle(1, 10), candle(2, 20)]);

        assert!(!series.snapshot().older_exhausted());
    }

    #[test]
    fn moving_newer_should_clear_exhausted_flag_after_evicting_the_oldest_candle() {
        let mut series = series(2);
        series.merge_initial([candle(1, 10), candle(2, 20)]);
        series.mark_older_exhausted();

        series.merge_newer([candle(3, 30)]);

        assert!(!series.snapshot().older_exhausted());
    }

    #[test]
    fn live_updates_should_not_break_a_historical_window() {
        let mut series = series(2);
        series.merge_initial([candle(2, 20), candle(3, 30)]);
        series.merge_older([candle(1, 10)], NewestCandle::Evicted);

        let candle = candle(4, 40);
        let changed = series.upsert_live(&candle);

        assert_eq!(
            (changed, series.snapshot().latest_timestamp()),
            (false, Some(2))
        );
    }

    #[test]
    fn merge_older_should_keep_live_edge_when_capacity_has_room() {
        let mut series = series(3);
        series.merge_initial([candle(2, 20), candle(3, 30)]);

        series.merge_older([candle(1, 10)], NewestCandle::Kept);
        let changed = series.upsert_live(&candle(4, 40));

        assert!(changed && series.at_live_edge());
        assert_eq!(series.snapshot().latest_timestamp(), Some(4));
    }

    /// A page merged with the newest candle kept never evicts: at capacity it
    /// fits nothing and says so, and the candle the chart is following is still
    /// there afterwards.
    ///
    /// This is the case that made an eviction policy of its own unsound: the
    /// newest candle is on screen, so losing it would move the chart out from
    /// under the user — and the series would leave the live edge, after which a
    /// live tick is refused forever.
    #[test]
    fn a_page_merged_with_the_newest_candle_kept_should_never_evict() {
        let mut series = series(3);
        series.merge_initial([candle(3, 30), candle(4, 40), candle(5, 50)]);
        let before = timestamps(&mut series);

        let merged = series.merge_older([candle(1, 10), candle(2, 20)], NewestCandle::Kept);

        assert_eq!(merged, OlderMerge::TrimmedByCapacity);
        assert_eq!(
            timestamps(&mut series),
            before,
            "a full buffer refuses the page rather than giving up the newest candle"
        );
        assert!(
            series.at_live_edge(),
            "nothing was evicted, so the series still follows the live edge"
        );
    }

    /// The page is trimmed to the room left, keeping the candles nearest the
    /// buffer: the ones that join it without a gap.
    #[test]
    fn a_page_merged_with_the_newest_candle_kept_should_keep_the_nearest_older_candles() {
        let mut series = series(5);
        series.merge_initial([candle(10, 10), candle(11, 11), candle(12, 12)]);

        let merged = series.merge_older(
            [
                candle(5, 5),
                candle(6, 6),
                candle(7, 7),
                candle(8, 8),
                candle(9, 9),
            ],
            NewestCandle::Kept,
        );

        assert_eq!(merged, OlderMerge::Advanced);
        assert_eq!(
            timestamps(&mut series),
            vec![8, 9, 10, 11, 12],
            "the two that fitted are the two closest to the buffer, not the first two of the page"
        );
        assert!(series.at_live_edge());
    }

    /// The opposite intent: the newest candle is off screen, so evicting it is
    /// what moves the series back in time — and what takes it off the live edge.
    #[test]
    fn a_page_merged_with_the_newest_candle_evicted_should_clear_the_live_edge() {
        let mut series = series(3);
        series.merge_initial([candle(3, 30), candle(4, 40), candle(5, 50)]);

        let merged = series.merge_older([candle(2, 20)], NewestCandle::Evicted);

        assert_eq!(merged, OlderMerge::Advanced);
        assert_eq!(timestamps(&mut series), vec![2, 3, 4]);
        assert!(!series.at_live_edge());
    }

    #[test]
    fn an_empty_older_page_should_make_no_progress_under_either_intent() {
        let mut kept = series(3);
        kept.merge_initial([candle(1, 10)]);
        let mut evicted = series(3);
        evicted.merge_initial([candle(1, 10)]);

        assert_eq!(
            kept.merge_older([], NewestCandle::Kept),
            OlderMerge::MadeNoProgress
        );
        assert_eq!(
            evicted.merge_older([], NewestCandle::Evicted),
            OlderMerge::MadeNoProgress
        );
        assert!(kept.at_live_edge() && evicted.at_live_edge());
    }

    /// A page carrying only candles the series already holds changed nothing, so
    /// it is no progress under either intent — and under the evicting one it must
    /// not move the edge either.
    #[test]
    fn a_page_of_candles_already_held_should_make_no_progress() {
        let mut kept = series(4);
        kept.merge_initial([candle(1, 10), candle(2, 20)]);
        let mut evicted = series(4);
        evicted.merge_initial([candle(1, 10), candle(2, 20)]);

        assert_eq!(
            kept.merge_older([candle(2, 20), candle(1, 10)], NewestCandle::Kept),
            OlderMerge::MadeNoProgress
        );
        assert_eq!(
            evicted.merge_older([candle(2, 20), candle(1, 10)], NewestCandle::Evicted),
            OlderMerge::MadeNoProgress
        );
        assert!(kept.at_live_edge() && evicted.at_live_edge());
    }

    #[test]
    fn fetched_history_should_keep_only_the_newest_rows_at_the_boundary() {
        let history = [5, 1, 4, 2, 3]
            .into_iter()
            .map(|timestamp| candle(timestamp, timestamp as u64))
            .collect();

        let bounded = bounded_history(history, 2);

        assert_eq!(
            bounded
                .into_iter()
                .map(|candle| candle.timestamp)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
    }

    #[test]
    fn fetched_history_should_deduplicate_before_applying_the_boundary() {
        let history = [(1, 1), (2, 2), (2, 20), (3, 3), (3, 30), (4, 4)]
            .into_iter()
            .map(|(timestamp, close)| candle(timestamp, close))
            .collect();

        let bounded = bounded_history(history, 3);

        assert_eq!(
            bounded
                .into_iter()
                .map(|candle| (candle.timestamp, CandlePrices::of(&candle).close()))
                .collect::<Vec<_>>(),
            vec![
                (2, Price::from(20_u64)),
                (3, Price::from(30_u64)),
                (4, Price::from(4_u64)),
            ],
            "the last value of a repeated timestamp wins, as it does when the page is merged"
        );
    }

    #[test]
    fn identical_live_update_should_not_invalidate_the_snapshot() {
        let mut series = series(2);
        series.merge_initial([candle(1, 10), candle(2, 20)]);
        let before = series.snapshot();

        let candle = candle(2, 20);
        let changed = series.upsert_live(&candle);
        let after = series.snapshot();

        assert!(!changed && Arc::ptr_eq(&before.candles, &after.candles));
    }

    /// The unit is part of a candle, so two candles of one timestamp that were
    /// published in different units are different candles.
    #[test]
    fn a_live_update_should_notice_a_changed_quantity_unit() {
        let mut series = series(2);
        series.merge_initial([candle(1, 10)]);
        let mut updated = candle(1, 10);
        updated.traded_quantity = Some(MarketQuantity {
            value: Quantity::from(1_u64),
            unit: QuantityUnit::AssetUnits {
                asset: test_support::token(test_support::VENUE, "BTC"),
            },
        });

        let changed = series.upsert_live(&updated);

        assert!(changed);
        assert_eq!(
            series.snapshot().latest().and_then(|candle| candle
                .traded_quantity
                .as_ref()
                .map(|quantity| quantity.unit.clone())),
            updated
                .traded_quantity
                .as_ref()
                .map(|quantity| quantity.unit.clone())
        );
    }

    #[test]
    fn metadata_only_change_should_reuse_the_snapshot_allocation() {
        let mut series = series(2);
        series.merge_initial([candle(1, 10), candle(2, 20)]);
        let before = series.snapshot();

        series.mark_older_exhausted();
        let after = series.snapshot();

        assert!(Arc::ptr_eq(&before.candles, &after.candles));
    }

    #[test]
    fn snapshot_should_preserve_market_price_tick() {
        let mut series = series(10);
        let expected = precision().price_tick;
        let snapshot = series.snapshot();

        assert_eq!(snapshot.price_tick(), expected);
    }

    #[test]
    fn exhausting_history_should_not_count_as_a_chart_change() {
        let mut series = series(2);
        series.merge_initial([candle(1, 10)]);
        let before = series.snapshot();

        series.mark_older_exhausted();
        let after = series.snapshot();

        assert!(
            before.shares_chart_data_with(&after),
            "running out of history says what more can be fetched, not what is drawn"
        );
    }

    #[test]
    fn an_empty_series_should_snapshot_without_candles() {
        let mut series = series(2);
        let snapshot = series.snapshot();

        assert!(snapshot.candles().is_empty());
        assert_eq!(snapshot.earliest_timestamp(), None);
        assert_eq!(snapshot.latest(), None);
    }

    #[test]
    fn a_zero_capacity_series_should_still_hold_one_candle() {
        let mut series = series(0);

        series.merge_initial([candle(1, 10), candle(2, 20)]);

        assert_eq!(timestamps(&mut series), vec![2]);
    }

    /// Every candle of one series names the listing it was published for, and the
    /// chart keeps that key: two listings of one instrument are two series.
    #[test]
    fn a_series_should_keep_the_listing_each_candle_names() {
        let mut series = series(4);
        let mut other = candle(3, 30);
        other.listing = test_support::listing_key(&test_support::spot_route(), "ETHUSDT");
        series.merge_initial([candle(1, 10), other.clone()]);

        let snapshot = series.snapshot();

        assert_eq!(snapshot.candles()[1].listing, other.listing);
        assert_ne!(snapshot.candles()[0].listing, other.listing);
    }

    #[test]
    fn candle_prices_should_read_the_ohlc_of_a_listing_candle() {
        let candle = ListingCandlestick {
            listing: listing(),
            open: market_price(Price::from(100_u64)),
            high: market_price(Price::from(110_u64)),
            low: market_price(Price::from(90_u64)),
            close: market_price(Price::from(105_u64)),
            traded_quantity: None,
            traded_notional: None,
            timestamp: 1,
        };

        let prices = CandlePrices::of(&candle);

        assert_eq!(
            (prices.open(), prices.high(), prices.low(), prices.close()),
            (
                Price::from(100_u64),
                Price::from(110_u64),
                Price::from(90_u64),
                Price::from(105_u64),
            )
        );
        assert!(prices.is_up());
        assert!(!CandlePrices::of(&candle_with_close(100, 99)).is_up());
    }

    fn candle_with_close(open: u64, close: u64) -> ListingCandlestick {
        let mut candle = candle(1, open);
        candle.close = market_price(Price::from(close));
        candle
    }
}
