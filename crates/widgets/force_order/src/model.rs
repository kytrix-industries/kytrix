use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use market_participants_model::{
    listing::rules::ListingPrecision, market::book::ListingForceOrder,
};
use smol_str::{SmolStr, ToSmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};

pub(super) const MAX_FORCE_ORDERS: usize = 200;

pub(super) type ForceOrderListingLabel = Option<SmolStr>;

#[derive(Clone, Debug)]
pub(super) struct ForceOrderListing {
    pub label: ForceOrderListingLabel,
    pub precision: Option<ListingPrecision>,
}

#[derive(Clone, Debug)]
pub(super) struct ForceOrderEntry {
    pub order: ListingForceOrder,
    pub listing: ForceOrderListing,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ForceOrderSnapshot {
    orders: Arc<[Arc<ForceOrderRow>]>,
}

impl ForceOrderSnapshot {
    pub(super) fn orders(&self) -> &[Arc<ForceOrderRow>] {
        &self.orders
    }

    pub(super) fn len(&self) -> usize {
        self.orders.len()
    }
}

impl PartialEq for ForceOrderSnapshot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.orders, &other.orders)
    }
}

impl Eq for ForceOrderSnapshot {}

#[derive(Debug)]
pub(super) struct ForceOrderRow {
    order: ListingForceOrder,
    listing: ForceOrderListing,
    display: Mutex<Option<(UtcOffset, Arc<ForceOrderDisplay>)>>,
}

impl ForceOrderRow {
    pub(super) const fn order(&self) -> &ListingForceOrder {
        &self.order
    }

    pub(super) fn listing_label(&self) -> &ForceOrderListingLabel {
        &self.listing.label
    }

    pub(super) fn display(&self, zone: UtcOffset) -> Arc<ForceOrderDisplay> {
        let mut cached = self.display.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached_zone, display)) = cached.as_ref()
            && *cached_zone == zone
        {
            return Arc::clone(display);
        }

        let display = Arc::new(ForceOrderDisplay::new(&self.order, &self.listing, zone));
        *cached = Some((zone, Arc::clone(&display)));
        display
    }
}

#[derive(Debug)]
pub(super) struct ForceOrderDisplay {
    pub price: SmolStr,
    pub quantity: SmolStr,
    pub time: SmolStr,
}

impl ForceOrderDisplay {
    fn new(order: &ListingForceOrder, listing: &ForceOrderListing, zone: UtcOffset) -> Self {
        let (price, quantity) = match listing.precision {
            Some(precision) => (
                order
                    .price
                    .value
                    .format_and_round_tick(precision.price_tick),
                order
                    .quantity
                    .value
                    .format_and_round_tick(precision.quantity_tick),
            ),
            None => (
                order.price.value.to_smolstr(),
                order.quantity.value.to_smolstr(),
            ),
        };
        Self {
            price,
            quantity,
            time: format_time(order.timestamp, zone),
        }
    }
}

pub(super) struct ForceOrderState {
    orders: BTreeMap<(i64, u64), Arc<ForceOrderRow>>,
    snapshot_orders: Arc<[Arc<ForceOrderRow>]>,
    snapshot_dirty: bool,
    next_id: u64,
    maximum_orders: usize,
}

impl ForceOrderState {
    pub(super) fn new(maximum_orders: usize) -> Self {
        Self {
            orders: BTreeMap::new(),
            snapshot_orders: Arc::from([]),
            snapshot_dirty: false,
            next_id: 1,
            maximum_orders: maximum_orders.clamp(1, MAX_FORCE_ORDERS),
        }
    }

    pub(super) fn merge(&mut self, orders: &[ForceOrderEntry]) -> usize {
        let mut retained = 0;
        for entry in orders {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            let key = (entry.order.timestamp, id);
            if self.orders.len() == self.maximum_orders {
                if self
                    .orders
                    .first_key_value()
                    .is_some_and(|(oldest, _)| key < *oldest)
                {
                    continue;
                }
                self.orders.pop_first();
            }

            self.orders.insert(
                key,
                Arc::new(ForceOrderRow {
                    order: entry.order.clone(),
                    listing: entry.listing.clone(),
                    display: Mutex::new(None),
                }),
            );
            retained += 1;
        }

        if retained > 0 {
            self.snapshot_dirty = true;
        }
        retained
    }

    pub(super) fn snapshot(&mut self) -> ForceOrderSnapshot {
        if self.snapshot_dirty {
            let mut orders = Vec::with_capacity(self.orders.len());
            orders.extend(self.orders.values().cloned());
            self.snapshot_orders = orders.into();
            self.snapshot_dirty = false;
        }
        ForceOrderSnapshot {
            orders: self.snapshot_orders.clone(),
        }
    }
}

fn format_time(timestamp: i64, zone: UtcOffset) -> SmolStr {
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
        decimal::{Price, Quantity},
        listing::route::VenueListingKey,
        market::{
            price::MarketPrice,
            units::{MarketQuantity, PositionSide, QuantityUnit},
        },
    };

    use super::*;

    fn listing(scope: &str, symbol: &str) -> VenueListingKey {
        test_support::listing_key(&test_support::route("binance", scope), symbol)
    }

    fn order(scope: &str, symbol: &str, timestamp: i64) -> ListingForceOrder {
        ListingForceOrder {
            listing: listing(scope, symbol),
            price: MarketPrice {
                value: Price::from(100_u64),
                quote_asset: test_support::token("fixture", "USDT"),
                basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
            },
            quantity: MarketQuantity::traded(Quantity::from(2_u64), QuantityUnit::Contracts)
                .expect("a contract count is a traded quantity"),
            position_side: PositionSide::Long,
            timestamp: (timestamp),
        }
    }

    fn entry(scope: &str, symbol: &str, timestamp: i64) -> ForceOrderEntry {
        ForceOrderEntry {
            order: order(scope, symbol, timestamp),
            listing: ForceOrderListing {
                label: Some(SmolStr::new(symbol)),
                precision: Some(precision()),
            },
        }
    }

    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse().expect("a price tick should parse"),
            quantity_tick: "0.001".parse().expect("a quantity step should parse"),
        }
    }

    #[test]
    fn merge_should_keep_only_the_newest_orders_in_chronological_order() {
        let mut state = ForceOrderState::new(2);

        state.merge(&[
            entry("perpetual", "BTCUSDT", 3),
            entry("perpetual", "BTCUSDT", 1),
            entry("perpetual", "BTCUSDT", 2),
        ]);
        let snapshot = state.snapshot();

        assert_eq!(
            snapshot
                .orders()
                .iter()
                .map(|row| row.order().timestamp)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn stale_batch_should_not_invalidate_a_full_snapshot() {
        let mut state = ForceOrderState::new(2);
        state.merge(&[
            entry("perpetual", "BTCUSDT", 2),
            entry("perpetual", "BTCUSDT", 3),
        ]);
        let before = state.snapshot();

        let retained = state.merge(&[entry("perpetual", "BTCUSDT", 1)]);
        let after = state.snapshot();

        assert!(retained == 0 && Arc::ptr_eq(&before.orders, &after.orders));
    }

    #[test]
    fn identical_orders_should_remain_distinct_without_a_source_identity() {
        let mut state = ForceOrderState::new(2);
        let order = entry("perpetual", "BTCUSDT", 1);

        let retained = state.merge(&[order.clone(), order]);
        let snapshot = state.snapshot();

        assert_eq!((retained, snapshot.len()), (2, 2));
    }

    #[test]
    fn two_listings_of_one_instrument_should_stay_distinct_rows() {
        let mut state = ForceOrderState::new(2);
        state.merge(&[
            entry("perpetual", "BTCUSDT", 1),
            entry("core-perp", "BTCUSDT", 1),
        ]);
        let snapshot = state.snapshot();

        assert_eq!(snapshot.len(), 2);
        assert_ne!(
            snapshot.orders()[0].order().listing,
            snapshot.orders()[1].order().listing
        );
    }

    #[test]
    fn row_display_should_be_built_once_per_zone() {
        let mut state = ForceOrderState::new(1);
        state.merge(&[entry("perpetual", "BTCUSDT", 1)]);
        let snapshot = state.snapshot();
        let row = &snapshot.orders()[0];
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
    fn row_display_should_write_the_numbers_at_the_listings_precision() {
        let mut state = ForceOrderState::new(1);
        let mut order = entry("perpetual", "BTCUSDT", 1);
        order.order.price.value = Price::from(90_000_u64);
        state.merge(&[order]);
        let snapshot = state.snapshot();
        let display = snapshot.orders()[0].display(UtcOffset::UTC);

        assert_eq!(
            [display.price.as_str(), display.quantity.as_str()],
            ["90000.00", "2.000"]
        );
    }

    #[test]
    fn row_display_should_state_the_number_without_a_published_precision() {
        let mut state = ForceOrderState::new(1);
        let mut order = entry("perpetual", "BTCUSDT", 1);
        order.listing.precision = None;
        state.merge(&[order]);
        let snapshot = state.snapshot();
        let display = snapshot.orders()[0].display(UtcOffset::UTC);

        assert_eq!(
            [display.price.as_str(), display.quantity.as_str()],
            ["100", "2"]
        );
    }

    #[test]
    fn empty_snapshot_should_be_ready_for_use_before_the_first_batch() {
        let mut state = ForceOrderState::new(1);
        let snapshot = state.snapshot();

        assert_eq!(snapshot.len(), 0);
        assert!(snapshot.orders().is_empty());
    }

    #[test]
    fn a_snapshot_should_be_the_same_read_only_for_the_same_retained_set() {
        let mut state = ForceOrderState::new(2);
        state.merge(&[entry("perpetual", "BTCUSDT", 1)]);
        let first = state.snapshot();
        let again = state.snapshot();

        assert_eq!(first, again, "an unchanged retained set is the same read");

        state.merge(&[entry("perpetual", "BTCUSDT", 2)]);
        let changed = state.snapshot();

        assert_ne!(first, changed, "a changed retained set is another read");
    }
}
