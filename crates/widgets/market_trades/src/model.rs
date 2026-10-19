use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
};

use gpui_kit::SharedString;
use market_participants_model::{
    decimal::UnixMillis,
    listing::rules::ListingPrecision,
    market::{book::ListingTrade, units::MarketTradeId},
};
use smol_str::format_smolstr;
use time::{OffsetDateTime, UtcOffset};

/// How many trades the tape keeps. Older ones are evicted as newer arrive.
pub(super) const MAX_TRADES: usize = 200;

/// An immutable, cheap-to-clone view of the retained trades, oldest first.
#[derive(Clone, Debug)]
pub(super) struct MarketTradesSnapshot {
    rows: Arc<[Arc<MarketTradeRow>]>,
}

impl MarketTradesSnapshot {
    pub(super) fn rows(&self) -> &[Arc<MarketTradeRow>] {
        &self.rows
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
}

/// One retained trade, with the labels it is drawn with filled in on demand.
#[derive(Debug)]
pub(super) struct MarketTradeRow {
    trade: ListingTrade,
    /// The increments of the listing this trade printed on.
    ///
    /// The row keeps them because its labels are built long after the generation
    /// that resolved the listing was published.
    precision: ListingPrecision,
    display: Mutex<Option<(UtcOffset, Arc<MarketTradeDisplay>)>>,
}

impl MarketTradeRow {
    pub(super) const fn trade(&self) -> &ListingTrade {
        &self.trade
    }

    /// The formatted labels of this row, built once per display zone and shared
    /// afterwards.
    ///
    /// Formatting is deferred rather than done on arrival: a trade that is
    /// evicted before it is ever drawn never costs a single label. The zone the
    /// time was read in is remembered beside the labels, so a frame that draws the
    /// same zone reuses them and a frame after the setting moved reads them again.
    pub(super) fn display(&self, zone: UtcOffset) -> Arc<MarketTradeDisplay> {
        let mut cached = self.display.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_zone, display)) = cached.as_ref()
            && *cached_zone == zone
        {
            return Arc::clone(display);
        }

        let display = Arc::new(MarketTradeDisplay::new(&self.trade, self.precision, zone));
        *cached = Some((zone, Arc::clone(&display)));
        display
    }
}

/// The values one row draws, without their units.
///
/// A price is quoted in the asset the trade carries and a quantity is counted in
/// the unit the trade carries, so both labels are attached where they are drawn:
/// only the numbers are a property of the row itself.
///
/// Both numbers are written at the precision of the listing the trade printed on,
/// so a price of `90000` on a listing with a tick of `0.01` reads as `90000.00`
/// rather than as the canonical form that drops the zeroes.
#[derive(Debug)]
pub(super) struct MarketTradeDisplay {
    pub price: SharedString,
    pub quantity: SharedString,
    pub time: SharedString,
}

impl MarketTradeDisplay {
    fn new(trade: &ListingTrade, precision: ListingPrecision, zone: UtcOffset) -> Self {
        Self {
            price: SharedString::new(
                trade
                    .price
                    .value
                    .format_and_round_tick(precision.price_tick)
                    .as_str(),
            ),
            quantity: SharedString::new(
                trade
                    .quantity
                    .value
                    .format_and_round_tick(precision.quantity_tick)
                    .as_str(),
            ),
            time: format_time(trade.timestamp, zone),
        }
    }
}

/// The trades the tape holds, kept in the order they printed.
pub(super) struct MarketTradesState {
    /// Keyed by chronological position rather than by the venue id.
    ///
    /// A trade id is unique for a venue and listing but is not required to increase
    /// with the execution timestamp, so time orders the tape and the id only breaks
    /// ties inside one millisecond. Keying on the pair is also what makes the map's
    /// own order the display order: the row to evict is simply the first entry.
    trades: BTreeMap<(UnixMillis, MarketTradeId), Arc<MarketTradeRow>>,
    /// The venue ids the tape already holds.
    ///
    /// The identifier is what tells two reports of the same execution apart, and
    /// it is the one thing the position key cannot answer: a venue that stamps
    /// the opening page and the live event differently would otherwise keep both
    /// copies of one trade.
    retained_ids: BTreeSet<MarketTradeId>,
    snapshot_rows: Arc<[Arc<MarketTradeRow>]>,
    snapshot_dirty: bool,
    maximum_trades: usize,
    /// The increments of the listing of this tape.
    ///
    /// The whole tape belongs to one listing — a stream is opened for exactly one
    /// — so every row of it is written at this precision.
    precision: ListingPrecision,
}

impl MarketTradesState {
    pub(super) fn new(maximum_trades: usize, precision: ListingPrecision) -> Self {
        Self {
            trades: BTreeMap::new(),
            retained_ids: BTreeSet::new(),
            snapshot_rows: Arc::from([]),
            snapshot_dirty: false,
            maximum_trades: maximum_trades.clamp(1, MAX_TRADES),
            precision,
        }
    }

    /// Inserts a batch, keeping only the newest `maximum_trades` trades.
    ///
    /// Returns how many rows the tape gained, so a caller can tell a batch that
    /// changed what is drawn from one that only repeated trades it already holds.
    pub(super) fn merge(&mut self, trades: impl IntoIterator<Item = ListingTrade>) -> usize {
        trades
            .into_iter()
            .filter(|trade| self.insert(trade))
            .count()
    }

    /// Inserts one trade, reporting whether it became a row.
    pub(super) fn insert(&mut self, trade: &ListingTrade) -> bool {
        if self.retained_ids.contains(&trade.id) {
            // The same execution reported twice — the opening page and a buffered
            // live event overlapping — is one row, and the copy that arrived first
            // is the one kept.
            return false;
        }

        let key = (trade.timestamp, trade.id.clone());
        if self.trades.len() == self.maximum_trades {
            if self
                .trades
                .first_key_value()
                .is_some_and(|(oldest, _)| key < *oldest)
            {
                // Older than everything retained: taking it would mean dropping a
                // newer row to make room for a stale one.
                return false;
            }
            if let Some((_, evicted)) = self.trades.pop_first() {
                self.retained_ids.remove(&evicted.trade().id);
            }
        }

        self.retained_ids.insert(trade.id.clone());
        self.trades.insert(
            key,
            Arc::new(MarketTradeRow {
                trade: trade.clone(),
                precision: self.precision,
                display: Mutex::new(None),
            }),
        );
        self.snapshot_dirty = true;
        true
    }

    /// The whole tape as one immutable value.
    ///
    /// A snapshot that nothing invalidated shares its row storage with the
    /// previous one, so publishing it again costs a reference count.
    pub(super) fn snapshot(&mut self) -> MarketTradesSnapshot {
        if self.snapshot_dirty {
            let mut rows = Vec::with_capacity(self.trades.len());
            rows.extend(self.trades.values().cloned());
            self.snapshot_rows = rows.into();
            self.snapshot_dirty = false;
        }

        MarketTradesSnapshot {
            rows: self.snapshot_rows.clone(),
        }
    }
}

/// The time of a trade, read in the zone the settings display data in.
fn format_time(timestamp: UnixMillis, zone: UtcOffset) -> SharedString {
    // The offset can move an instant past the last date the calendar holds — a
    // timestamp at the far end of the range is one the service cannot publish, and
    // it is written as a placeholder rather than taken to a date that is not one.
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .and_then(|datetime| datetime.checked_to_offset(zone))
        .map_or_else(
            || SharedString::new_static("--:--:--.---"),
            |datetime| {
                SharedString::new(
                    format_smolstr!(
                        "{:02}:{:02}:{:02}.{:03}",
                        datetime.hour(),
                        datetime.minute(),
                        datetime.second(),
                        datetime.millisecond()
                    )
                    .as_str(),
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        asset::AssetId,
        decimal::{Price, Quantity, Tick},
        listing::{route::VenueListingKey, venue::PriceBasis},
        market::{
            price::MarketPrice,
            units::{MarketQuantity, QuantityUnit, Side},
        },
    };

    use super::*;

    fn price(value: u64, quote: AssetId) -> MarketPrice {
        MarketPrice {
            value: Price::from(value),
            quote_asset: quote,
            basis: PriceBasis::PerAssetUnit,
        }
    }

    fn quantity(value: u64, unit: QuantityUnit) -> MarketQuantity {
        MarketQuantity {
            value: Quantity::from(value),
            unit,
        }
    }

    fn listing() -> VenueListingKey {
        test_support::listing_key(&test_support::spot_route(), "BTCUSDT")
    }

    fn trade(id: u64, timestamp: UnixMillis, price_value: u64) -> ListingTrade {
        ListingTrade {
            listing: listing(),
            id: MarketTradeId::Numeric(id),
            price: price(price_value, test_support::currency("USD")),
            quantity: quantity(
                1,
                QuantityUnit::AssetUnits {
                    asset: test_support::token("binance", "BTC"),
                },
            ),
            side: Side::Buy,
            timestamp,
        }
    }

    fn state(maximum_trades: usize) -> MarketTradesState {
        MarketTradesState::new(maximum_trades, precision())
    }

    /// A price tick of one cent and a quantity step of one thousandth, so a label
    /// that keeps the listing's precision is told apart from the canonical form.
    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse::<Tick>().expect("a price tick should parse"),
            quantity_tick: "0.001"
                .parse::<Tick>()
                .expect("a quantity step should parse"),
        }
    }

    fn ordered_ids(trades: &mut MarketTradesState) -> Vec<MarketTradeId> {
        trades
            .snapshot()
            .rows()
            .iter()
            .map(|row| row.trade().id.clone())
            .collect()
    }

    fn numeric(ids: impl IntoIterator<Item = u64>) -> Vec<MarketTradeId> {
        ids.into_iter().map(MarketTradeId::Numeric).collect()
    }

    #[test]
    fn merge_should_order_the_tape_by_timestamp() {
        let mut trades = state(10);

        let inserted = trades.merge([trade(30, 10, 101), trade(10, 30, 103), trade(20, 20, 102)]);

        assert_eq!(inserted, 3);
        assert_eq!(ordered_ids(&mut trades), numeric([30, 20, 10]));
    }

    #[test]
    fn insert_should_keep_the_first_copy_of_a_trade_the_tape_already_holds() {
        let mut trades = state(10);
        assert!(trades.insert(&trade(7, 10, 100)));

        // The same venue id, stamped differently by the source that reported it.
        assert!(!trades.insert(&trade(7, 20, 999)));

        let snapshot = trades.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.rows()[0].trade().price.value, Price::from(100_u64));
    }

    /// A venue identifies a trade either by a number or by its own identifier, and
    /// the two are different trades even when they read the same.
    #[test]
    fn insert_should_keep_a_native_id_apart_from_the_numeric_one_of_the_same_digits() {
        let mut trades = state(10);
        let mut native = trade(7, 10, 100);
        native.id = MarketTradeId::Native(
            market_participants_model::identifier::IdentifierValue::new("7")
                .expect("test id should decode"),
        );
        assert!(trades.insert(&trade(7, 10, 100)));
        assert!(trades.insert(&native));

        let snapshot = trades.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(
            snapshot.rows()[0].trade().id,
            MarketTradeId::Numeric(7),
            "the tape keeps both reports of the same digit"
        );
    }

    #[test]
    fn merge_should_keep_distinct_ids_with_the_same_timestamp() {
        let mut trades = state(10);

        trades.merge([trade(2, 100, 102), trade(1, 100, 101)]);

        assert_eq!(ordered_ids(&mut trades), numeric([1, 2]));
    }

    #[test]
    fn insert_should_evict_the_oldest_trade_at_capacity() {
        let mut trades = state(3);
        trades.merge([
            trade(900, 30, 103),
            trade(100, 10, 101),
            trade(500, 20, 102),
            trade(1, 40, 104),
        ]);

        assert_eq!(ordered_ids(&mut trades), numeric([500, 900, 1]));
    }

    #[test]
    fn insert_should_drop_a_trade_older_than_everything_retained() {
        let mut trades = state(2);
        trades.merge([trade(10, 10, 100), trade(11, 11, 101)]);

        assert!(!trades.insert(&trade(999, 9, 99)));

        assert_eq!(ordered_ids(&mut trades), numeric([10, 11]));
    }

    #[test]
    fn new_should_cap_the_requested_capacity_at_maximum_trades() {
        let mut trades = state(MAX_TRADES + 10);
        trades.merge((0..MAX_TRADES + 5).map(|id| trade(id as u64, id as i64, id as u64)));

        let snapshot = trades.snapshot();
        assert_eq!(snapshot.len(), MAX_TRADES);
        assert_eq!(snapshot.rows()[0].trade().id, MarketTradeId::Numeric(5));
    }

    #[test]
    fn unchanged_snapshot_should_reuse_immutable_row_storage() {
        let mut trades = state(2);
        trades.merge([trade(10, 10, 100), trade(11, 11, 101)]);
        let before = trades.snapshot();

        assert!(!trades.insert(&trade(9, 9, 99)));
        let after = trades.snapshot();

        assert!(Arc::ptr_eq(&before.rows, &after.rows));
    }

    #[test]
    fn an_id_evicted_from_the_tape_should_be_accepted_again() {
        let mut trades = state(2);
        trades.merge([trade(1, 10, 100), trade(2, 20, 101)]);
        trades.merge([trade(3, 30, 102)]);

        // Id 1 left the tape with the row that held it, so nothing about it is
        // still remembered.
        assert!(trades.insert(&trade(1, 40, 103)));

        assert_eq!(ordered_ids(&mut trades), numeric([3, 1]));
    }

    #[test]
    fn row_display_should_be_initialized_once_on_demand() {
        let mut trades = state(1);
        trades.insert(&trade(1, 10, 100));
        let snapshot = trades.snapshot();
        let row = &snapshot.rows()[0];
        assert!(
            row.display
                .lock()
                .expect("the cache is not poisoned")
                .is_none()
        );

        let first = row.display(UtcOffset::UTC);
        let second = row.display(UtcOffset::UTC);
        let other = row.display(UtcOffset::from_hms(8, 0, 0).expect("UTC+8 is a valid offset"));

        assert!(Arc::ptr_eq(&first, &second), "the same zone reuses the row");
        assert_ne!(
            first.time, other.time,
            "another zone is another reading of the same instant"
        );
    }

    /// The display offset can move an instant past the last day the calendar holds.
    /// That is not a time this can write, so it is a placeholder rather than a panic.
    #[test]
    fn a_time_the_display_zone_cannot_hold_should_read_as_a_placeholder() {
        let boundary = time::Date::MAX
            .with_hms(23, 0, 0)
            .expect("the last day has an evening")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the last day is a millisecond timestamp");

        assert_eq!(
            format_time(
                timestamp,
                UtcOffset::from_hms(14, 0, 0).expect("UTC+14 is a valid offset")
            ),
            "--:--:--.---"
        );

        let boundary = time::Date::MIN
            .with_hms(0, 0, 0)
            .expect("the first day has a midnight")
            .assume_utc();
        let timestamp = i64::try_from(boundary.unix_timestamp_nanos() / 1_000_000)
            .expect("the first day is a millisecond timestamp");

        assert_eq!(
            format_time(
                timestamp,
                UtcOffset::from_hms(-12, 0, 0).expect("UTC-12 is a valid offset")
            ),
            "--:--:--.---",
            "a western offset runs off the other end of the calendar"
        );
    }

    /// A number is written at the precision of the listing it printed on, so a
    /// price that is a whole number keeps the decimals its tick states instead of
    /// being read as the canonical form that drops trailing zeroes.
    #[test]
    fn row_display_should_write_a_whole_price_at_the_listings_precision() {
        let mut trades = state(1);
        trades.insert(&trade(1, 1_000, 90_000));
        let snapshot = trades.snapshot();
        let display = snapshot.rows()[0].display(UtcOffset::UTC);

        assert_eq!(
            [
                display.price.as_ref(),
                display.quantity.as_ref(),
                display.time.as_ref(),
            ],
            ["90000.00", "1.000", "00:00:01.000"]
        );
    }

    /// A value the listing publishes at a finer increment than its own rule set
    /// states is read at that rule set, not at the digits the venue happened to
    /// print.
    #[test]
    fn row_display_should_round_the_values_to_the_listings_increments() {
        let mut trades = state(1);
        let mut exact = trade(1, 1_000, 123);
        exact.price.value = "123.456789".parse::<Price>().expect("price should parse");
        exact.quantity = quantity(
            3,
            QuantityUnit::AssetUnits {
                asset: test_support::token("binance", "BTC"),
            },
        );
        exact.quantity.value = "0.0004".parse::<Quantity>().expect("quantity should parse");
        trades.insert(&exact);
        let snapshot = trades.snapshot();
        let display = snapshot.rows()[0].display(UtcOffset::UTC);

        assert_eq!(
            [
                display.price.as_ref(),
                display.quantity.as_ref(),
                display.time.as_ref(),
            ],
            ["123.46", "0.000", "00:00:01.000"]
        );
    }
}
