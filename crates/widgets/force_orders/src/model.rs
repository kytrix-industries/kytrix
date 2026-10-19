//! The force orders a pane retains, and how one row reads.
//!
//! A liquidation is reported by a route-level stream: the venue says which of its
//! listings was liquidated, and the stream resolves that to the exact
//! [`VenueListingKey`] it belongs to. This module keeps the liquidation together
//! with what the published generation says about that listing — the name it is
//! shown under and the increments its numbers are written at — because a listing key
//! alone is not something a reader can recognise, and a liquidation carries no
//! precision of its own.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use gpui_kit::SharedString;
use market_participants_model::{
    listing::rules::ListingPrecision, market::book::ListingForceOrder,
};
use smol_str::{ToSmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};

pub(super) const MAX_FORCE_ORDERS: usize = 200;

/// The name one listing is shown under, when the published generation has one.
///
/// The name is read from the display catalog of one generation, so it is the same
/// name the selector and every other pane draws the listing with. A generation that
/// names no display for the listing is not a reason to invent one from the venue's
/// own symbol or from an identity: the row says it is unknown, localized by the
/// view, and the structured listing key stays in the order it came from.
pub(super) type ForceOrderListingLabel = Option<SharedString>;

/// What one published generation says about the listing a row draws.
///
/// The name and the increments are read from the same generation, so a row cannot
/// be named by one publication and written at the precision of another.
#[derive(Clone, Debug)]
pub(super) struct ForceOrderListing {
    pub label: ForceOrderListingLabel,
    /// The increments the listing's numbers are written at, or nothing when the
    /// generation does not hold its rules: the row then states the number in the
    /// canonical decimal form rather than holding back the liquidation.
    pub precision: Option<ListingPrecision>,
}

/// One liquidation together with what the published generation says about its
/// listing.
#[derive(Clone, Debug)]
pub(super) struct ForceOrderEntry {
    pub order: ListingForceOrder,
    pub listing: ForceOrderListing,
}

/// An immutable, cheap-to-clone view of the retained force orders.
#[derive(Clone, Debug, Default)]
pub(super) struct ForceOrdersSnapshot {
    orders: Arc<[Arc<ForceOrderRow>]>,
}

impl ForceOrdersSnapshot {
    pub(super) fn orders(&self) -> &[Arc<ForceOrderRow>] {
        &self.orders
    }

    pub(super) fn len(&self) -> usize {
        self.orders.len()
    }
}

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

    /// The text of a row that is produced only once something draws it.
    ///
    /// The zone the time was read in is remembered beside the text, so a frame that
    /// draws the same zone reuses it and a frame after the setting moved reads it
    /// again.
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
    /// The listing's published display name, or nothing when the generation that
    /// named it has none: the view localizes that case rather than the model
    /// inventing a name.
    pub listing: ForceOrderListingLabel,
    pub price: SharedString,
    pub quantity: SharedString,
    pub time: SharedString,
}

impl ForceOrderDisplay {
    fn new(order: &ListingForceOrder, listing: &ForceOrderListing, zone: UtcOffset) -> Self {
        // A liquidation is written at the increments of its own listing, so a price
        // of `90000` on a listing with a tick of `0.01` reads as `90000.00`.
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
            listing: listing.label.clone(),
            price: SharedString::new(price.as_str()),
            quantity: SharedString::new(quantity.as_str()),
            time: format_time(order.timestamp, zone),
        }
    }
}

pub(super) struct ForceOrdersState {
    orders: BTreeMap<(i64, u64), Arc<ForceOrderRow>>,
    snapshot_orders: Arc<[Arc<ForceOrderRow>]>,
    snapshot_dirty: bool,
    next_id: u64,
    maximum_orders: usize,
}

impl ForceOrdersState {
    pub(super) fn new(maximum_orders: usize) -> Self {
        Self {
            orders: BTreeMap::new(),
            snapshot_orders: Arc::from([]),
            snapshot_dirty: false,
            next_id: 1,
            maximum_orders: maximum_orders.clamp(1, MAX_FORCE_ORDERS),
        }
    }

    /// Inserts a batch, keeping only the newest `maximum_orders` entries.
    ///
    /// Returns the number of retained orders so callers can distinguish a
    /// batch that changed the visible set from one that was fully evicted.
    /// Orders carry no market-participant-assigned identity, so two identical reports
    /// stay distinct instead of silently collapsing into one row.
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

    pub(super) fn snapshot(&mut self) -> ForceOrdersSnapshot {
        if self.snapshot_dirty {
            let mut orders = Vec::with_capacity(self.orders.len());
            orders.extend(self.orders.values().cloned());
            self.snapshot_orders = orders.into();
            self.snapshot_dirty = false;
        }
        ForceOrdersSnapshot {
            orders: self.snapshot_orders.clone(),
        }
    }
}

/// The time of a liquidation, read in the zone the settings display data in.
fn format_time(timestamp: i64, zone: UtcOffset) -> SharedString {
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
                label: Some(SharedString::new(symbol)),
                precision: Some(precision()),
            },
        }
    }

    /// A price tick of one cent and a quantity step of one thousandth, so a label
    /// that keeps the listing's precision is told apart from the canonical form.
    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: "0.01".parse().expect("a price tick should parse"),
            quantity_tick: "0.001".parse().expect("a quantity step should parse"),
        }
    }

    #[test]
    fn merge_should_keep_only_the_newest_orders_in_chronological_order() {
        let mut state = ForceOrdersState::new(2);

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
        let mut state = ForceOrdersState::new(2);
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
        let mut state = ForceOrdersState::new(2);
        let order = entry("perpetual", "BTCUSDT", 1);

        let retained = state.merge(&[order.clone(), order]);
        let snapshot = state.snapshot();

        assert_eq!((retained, snapshot.len()), (2, 2));
    }

    /// Two listings of one canonical instrument are two rows: the key that decides
    /// which listing a liquidation belongs to is the exact listing, not the
    /// instrument it implements.
    #[test]
    fn two_listings_of_one_instrument_should_stay_distinct_rows() {
        let mut state = ForceOrdersState::new(2);
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
        let mut state = ForceOrdersState::new(1);
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

    /// A liquidation states its numbers at the increments of its own listing, so a
    /// whole price keeps the decimals its tick states.
    #[test]
    fn row_display_should_write_the_numbers_at_the_listings_precision() {
        let mut state = ForceOrdersState::new(1);
        let mut order = entry("perpetual", "BTCUSDT", 1);
        order.order.price.value = Price::from(90_000_u64);
        state.merge(&[order]);
        let snapshot = state.snapshot();
        let display = snapshot.orders()[0].display(UtcOffset::UTC);

        assert_eq!(
            [display.price.as_ref(), display.quantity.as_ref()],
            ["90000.00", "2.000"]
        );
    }

    /// A generation that does not hold the listing's rules is not a reason to hold
    /// back the liquidation: the row states the number it was given.
    #[test]
    fn row_display_should_state_the_number_without_a_published_precision() {
        let mut state = ForceOrdersState::new(1);
        let mut order = entry("perpetual", "BTCUSDT", 1);
        order.listing.precision = None;
        state.merge(&[order]);
        let snapshot = state.snapshot();
        let display = snapshot.orders()[0].display(UtcOffset::UTC);

        assert_eq!(
            [display.price.as_ref(), display.quantity.as_ref()],
            ["100", "2"]
        );
    }

    #[test]
    fn empty_snapshot_should_be_ready_for_use_before_the_first_batch() {
        let mut state = ForceOrdersState::new(1);
        let snapshot = state.snapshot();

        assert_eq!(snapshot.len(), 0);
        assert!(snapshot.orders().is_empty());
    }
}
