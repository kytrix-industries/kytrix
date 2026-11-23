use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
};

use market_participants_model::{
    decimal::UnixMillis,
    listing::rules::ListingPrecision,
    market::{book::ListingTrade, units::MarketTradeId},
};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};

pub(super) const MAX_TRADES: usize = 200;

#[derive(Clone, Debug, Default)]
pub(super) struct MarketTradeSnapshot {
    rows: Arc<[Arc<MarketTradeRow>]>,
}

impl MarketTradeSnapshot {
    pub(super) fn rows(&self) -> &[Arc<MarketTradeRow>] {
        &self.rows
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
}

impl PartialEq for MarketTradeSnapshot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.rows, &other.rows)
    }
}

impl Eq for MarketTradeSnapshot {}

#[derive(Debug)]
pub(super) struct MarketTradeRow {
    trade: ListingTrade,
    precision: ListingPrecision,
    display: Mutex<Option<(UtcOffset, Arc<MarketTradeDisplay>)>>,
}

impl MarketTradeRow {
    pub(super) const fn trade(&self) -> &ListingTrade {
        &self.trade
    }

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

#[derive(Debug)]
pub(super) struct MarketTradeDisplay {
    pub price: SmolStr,
    pub quantity: SmolStr,
    pub time: SmolStr,
}

impl MarketTradeDisplay {
    fn new(trade: &ListingTrade, precision: ListingPrecision, zone: UtcOffset) -> Self {
        Self {
            price: trade
                .price
                .value
                .format_and_round_tick(precision.price_tick),
            quantity: trade
                .quantity
                .value
                .format_and_round_tick(precision.quantity_tick),
            time: format_time(trade.timestamp, zone),
        }
    }
}

pub(super) struct MarketTradeState {
    trades: BTreeMap<(UnixMillis, MarketTradeId), Arc<MarketTradeRow>>,
    retained_ids: BTreeSet<MarketTradeId>,
    snapshot_rows: Arc<[Arc<MarketTradeRow>]>,
    snapshot_dirty: bool,
    maximum_trades: usize,
    precision: ListingPrecision,
}

impl MarketTradeState {
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

    pub(super) fn merge(&mut self, trades: impl IntoIterator<Item = ListingTrade>) -> usize {
        trades
            .into_iter()
            .filter(|trade| self.insert(trade))
            .count()
    }

    pub(super) fn insert(&mut self, trade: &ListingTrade) -> bool {
        if self.retained_ids.contains(&trade.id) {
            return false;
        }

        let key = (trade.timestamp, trade.id.clone());
        if self.trades.len() == self.maximum_trades {
            if self
                .trades
                .first_key_value()
                .is_some_and(|(oldest, _)| key < *oldest)
            {
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

    pub(super) fn snapshot(&mut self) -> MarketTradeSnapshot {
        if self.snapshot_dirty {
            let mut rows = Vec::with_capacity(self.trades.len());
            rows.extend(self.trades.values().cloned());
            self.snapshot_rows = rows.into();
            self.snapshot_dirty = false;
        }

        MarketTradeSnapshot {
            rows: self.snapshot_rows.clone(),
        }
    }
}

fn format_time(timestamp: UnixMillis, zone: UtcOffset) -> SmolStr {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp) * 1_000_000)
        .ok()
        .and_then(|datetime| datetime.checked_to_offset(zone))
        .map_or_else(
            || SmolStr::new_static("--:--:--.---"),
            |datetime| {
                format_smolstr!(
                    "{:02}:{:02}:{:02}.{:03}",
                    datetime.hour(),
                    datetime.minute(),
                    datetime.second(),
                    datetime.millisecond()
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

    fn state(maximum_trades: usize) -> MarketTradeState {
        MarketTradeState::new(maximum_trades, precision())
    }

    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse::<Tick>().expect("a price tick should parse"),
            quantity_tick: "0.001"
                .parse::<Tick>()
                .expect("a quantity step should parse"),
        }
    }

    fn ordered_ids(trades: &mut MarketTradeState) -> Vec<MarketTradeId> {
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

        assert!(!trades.insert(&trade(7, 20, 999)));

        let snapshot = trades.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.rows()[0].trade().price.value, Price::from(100_u64));
    }

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
    fn an_unchanged_snapshot_should_be_the_same_read() {
        let mut trades = state(2);
        trades.merge([trade(10, 10, 100), trade(11, 11, 101)]);
        let first = trades.snapshot();
        let second = trades.snapshot();

        assert_eq!(first, second);
        assert!(trades.insert(&trade(12, 12, 102)));
        assert_ne!(first, trades.snapshot());
    }

    #[test]
    fn an_id_evicted_from_the_tape_should_be_accepted_again() {
        let mut trades = state(2);
        trades.merge([trade(1, 10, 100), trade(2, 20, 101)]);
        trades.merge([trade(3, 30, 102)]);

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

    #[test]
    fn row_display_should_write_a_whole_price_at_the_listings_precision() {
        let mut trades = state(1);
        trades.insert(&trade(1, 1_000, 90_000));
        let snapshot = trades.snapshot();
        let display = snapshot.rows()[0].display(UtcOffset::UTC);

        assert_eq!(
            [
                display.price.as_str(),
                display.quantity.as_str(),
                display.time.as_str(),
            ],
            ["90000.00", "1.000", "00:00:01.000"]
        );
    }

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
                display.price.as_str(),
                display.quantity.as_str(),
                display.time.as_str(),
            ],
            ["123.46", "0.000", "00:00:01.000"]
        );
    }
}
