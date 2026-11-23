use std::{
    cmp::Ordering,
    sync::{Arc, OnceLock},
};

use ahash::HashMap;
use market_participants_connector::{market::error::MarketError, routing::LONGBRIDGE_US_SCOPE};
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Quantity, Tick},
    identifier::VenueId,
    listing::{route::VenueListingKey, rules::ListingPrecision},
    market::{selection::ListingTicker, units::MarketQuantity},
};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;

pub(super) const MAX_WATCH_LIST_ENTRIES: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatchListStatus {
    Connecting,
    LoadingSnapshot,
    Ready,
    Error,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(super) enum WatchListStreamError {
    #[error(transparent)]
    MarketParticipant(#[from] MarketError),
    #[error("the stream published a ticker of {actual:?} while {expected:?} was subscribed")]
    TickerListingMismatch {
        expected: Box<VenueListingKey>,
        actual: Box<VenueListingKey>,
    },
}

#[derive(Clone, Debug)]
pub(super) enum WatchListStreamEvent {
    Status(WatchListStatus),
    Snapshot {
        ticker: Arc<ListingTicker>,
        precision: ListingPrecision,
    },
    Error(WatchListStreamError),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TickerState {
    ticker: Arc<ListingTicker>,
    precision: ListingPrecision,
}

#[derive(Debug)]
pub(super) struct WatchListEntry {
    status: WatchListStatus,
    ticker: Option<TickerState>,
    pending_snapshot: Option<TickerState>,
    ready_received: bool,
    error: Option<WatchListStreamError>,
    row: Arc<WatchListRow>,
}

impl WatchListEntry {
    pub(super) fn connecting(key: &VenueListingKey) -> Self {
        Self {
            status: WatchListStatus::Connecting,
            ticker: None,
            pending_snapshot: None,
            ready_received: false,
            error: None,
            row: Arc::new(WatchListRow::new(key)),
        }
    }

    pub(super) fn apply_event(&mut self, event: &WatchListStreamEvent) -> bool {
        match event {
            WatchListStreamEvent::Status(status) => self.apply_status(*status),
            WatchListStreamEvent::Snapshot { ticker, precision } => {
                self.apply_snapshot(ticker.clone(), *precision)
            }
            WatchListStreamEvent::Error(error) => self.apply_error(error),
        }
    }

    pub(super) fn begin_retry(&mut self) -> bool {
        self.status = WatchListStatus::Connecting;
        self.pending_snapshot = None;
        self.ready_received = false;
        self.error = None;
        self.refresh_row();
        true
    }

    pub(super) fn status(&self) -> WatchListStatus {
        self.status
    }

    pub(super) fn error(&self) -> Option<&WatchListStreamError> {
        self.error.as_ref()
    }

    pub(super) fn row(&self) -> &Arc<WatchListRow> {
        &self.row
    }

    fn apply_status(&mut self, status: WatchListStatus) -> bool {
        match status {
            WatchListStatus::Connecting => false,
            WatchListStatus::LoadingSnapshot => {
                if matches!(self.status, WatchListStatus::Ready | WatchListStatus::Error) {
                    return false;
                }
                self.status = WatchListStatus::LoadingSnapshot;
                self.refresh_row();
                true
            }
            WatchListStatus::Ready => {
                if self.ready_received
                    || matches!(self.status, WatchListStatus::Ready | WatchListStatus::Error)
                {
                    return false;
                }
                self.ready_received = true;
                let Some(ticker) = self.pending_snapshot.take() else {
                    return false;
                };
                self.commit(ticker)
            }
            WatchListStatus::Error => false,
        }
    }

    fn apply_snapshot(&mut self, ticker: Arc<ListingTicker>, precision: ListingPrecision) -> bool {
        if self.status == WatchListStatus::Error {
            return false;
        }
        let state = TickerState { ticker, precision };
        if !self.ready_received {
            self.pending_snapshot = Some(state);
            return false;
        }
        self.commit(state)
    }

    fn apply_error(&mut self, error: &WatchListStreamError) -> bool {
        if self.status == WatchListStatus::Error && self.error.as_ref() == Some(error) {
            return false;
        }
        self.status = WatchListStatus::Error;
        self.pending_snapshot = None;
        self.ready_received = false;
        self.error = Some(error.clone());
        self.refresh_row();
        true
    }

    fn commit(&mut self, state: TickerState) -> bool {
        let unchanged = self.status == WatchListStatus::Ready
            && self
                .ticker
                .as_ref()
                .is_some_and(|current| current.ticker.as_ref() == state.ticker.as_ref());
        if unchanged {
            return false;
        }
        self.status = WatchListStatus::Ready;
        self.ticker = Some(state);
        self.error = None;
        self.refresh_row();
        true
    }

    fn refresh_row(&mut self) {
        self.row = Arc::new(WatchListRow::with_state(
            &self.row,
            self.status,
            self.ticker.clone(),
        ));
    }
}

#[derive(Debug)]
pub(super) struct WatchListRow {
    key: Arc<VenueListingKey>,
    market: WatchListMarket,
    state: WatchListRowState,
    display: OnceLock<WatchListRowDisplay>,
}

impl WatchListRow {
    fn new(key: &VenueListingKey) -> Self {
        Self::of(
            key,
            WatchListRowState::Ticker {
                status: WatchListStatus::Connecting,
                ticker: None,
            },
        )
    }

    pub(super) fn awaiting_market(key: &VenueListingKey) -> Self {
        Self::of(key, WatchListRowState::AwaitingMarket)
    }

    fn of(key: &VenueListingKey, state: WatchListRowState) -> Self {
        Self {
            key: Arc::new(key.clone()),
            market: WatchListMarket::of(key),
            state,
            display: OnceLock::new(),
        }
    }

    fn with_state(&self, status: WatchListStatus, ticker: Option<TickerState>) -> Self {
        Self {
            key: self.key.clone(),
            market: self.market,
            state: WatchListRowState::Ticker { status, ticker },
            display: OnceLock::new(),
        }
    }

    pub(super) fn key(&self) -> &Arc<VenueListingKey> {
        &self.key
    }

    pub(super) fn market(&self) -> WatchListMarket {
        self.market
    }

    pub(super) fn state(&self) -> &WatchListRowState {
        &self.state
    }

    pub(super) fn status(&self) -> WatchListStatus {
        match &self.state {
            WatchListRowState::AwaitingMarket => WatchListStatus::Connecting,
            WatchListRowState::Ticker { status, .. } => *status,
        }
    }

    pub(super) fn has_ticker(&self) -> bool {
        matches!(&self.state, WatchListRowState::Ticker { ticker, .. } if ticker.is_some())
    }

    pub(super) fn display(&self) -> Option<&WatchListRowDisplay> {
        let WatchListRowState::Ticker {
            ticker: Some(state),
            ..
        } = &self.state
        else {
            return None;
        };
        Some(
            self.display
                .get_or_init(|| WatchListRowDisplay::new(&state.ticker, state.precision)),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum WatchListRowState {
    AwaitingMarket,
    Ticker {
        status: WatchListStatus,
        ticker: Option<TickerState>,
    },
}

impl WatchListRowState {
    pub(super) const fn is_read(&self) -> bool {
        matches!(self, Self::Ticker { .. })
    }
}

#[derive(Debug)]
pub(super) struct WatchListRowDisplay {
    pub price: SmolStr,
    pub quote_asset: AssetId,
    pub change: SmolStr,
    pub change_percent: SmolStr,
    pub traded_quantity: Option<MarketQuantity>,
    pub traded_notional: Option<MarketQuantity>,
    pub direction: PriceDirection,
}

impl WatchListRowDisplay {
    fn new(ticker: &ListingTicker, precision: ListingPrecision) -> Self {
        let (change, change_percent) = format_change(ticker, precision.price_tick);

        Self {
            price: ticker
                .close
                .value
                .format_and_round_tick(precision.price_tick),
            quote_asset: ticker.close.quote_asset.clone(),
            change,
            change_percent,
            traded_quantity: ticker.traded_quantity.clone(),
            traded_notional: ticker.traded_notional.clone(),
            direction: PriceDirection::of(ticker),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PriceDirection {
    Up,
    Flat,
    Down,
}

impl PriceDirection {
    fn of(ticker: &ListingTicker) -> Self {
        match ticker.close.value.cmp(&ticker.open.value) {
            Ordering::Greater => Self::Up,
            Ordering::Equal => Self::Flat,
            Ordering::Less => Self::Down,
        }
    }
}

pub(super) fn format_change(ticker: &ListingTicker, price_tick: Tick) -> (SmolStr, SmolStr) {
    let current = ticker.close.value;
    let open = ticker.open.value;
    let (magnitude, is_negative) = if current >= open {
        (current.checked_sub(open).unwrap_or(Price::ZERO), false)
    } else {
        (open.checked_sub(current).unwrap_or(Price::ZERO), true)
    };
    let magnitude_text = magnitude.format_and_round_tick(price_tick);
    let change = if is_negative {
        format_smolstr!("-{magnitude_text}")
    } else {
        magnitude_text
    };

    let change_percent = if open == Price::ZERO {
        SmolStr::new_static("--")
    } else {
        let magnitude_percent = magnitude.inner() as f64 / open.inner() as f64 * 100.0;
        let percent = if is_negative {
            -magnitude_percent
        } else {
            magnitude_percent
        };
        format_smolstr!("{percent:.2}%")
    };

    (change, change_percent)
}

pub(super) fn format_volume(volume: Quantity) -> SmolStr {
    const SCALE: f64 = 1_000_000_000_000_000_000.0;
    let volume = volume.inner() as f64 / SCALE;
    if volume >= 1_000_000.0 {
        format_smolstr!("{:.2}M", volume / 1_000_000.0)
    } else if volume >= 1_000.0 {
        format_smolstr!("{:.2}K", volume / 1_000.0)
    } else {
        format_smolstr!("{volume:.2}")
    }
}

pub(super) fn sort_entries(entries: &mut [VenueListingKey]) {
    entries.sort_unstable();
}

pub(super) fn split_rows(
    keys: &[VenueListingKey],
    entries: &HashMap<VenueListingKey, WatchListEntry>,
) -> (Arc<[Arc<WatchListRow>]>, Arc<[Arc<WatchListRow>]>) {
    let mut continuous = Vec::with_capacity(keys.len());
    let mut session = Vec::with_capacity(keys.len());
    for key in keys {
        let row = entries.get(key).map_or_else(
            || Arc::new(WatchListRow::awaiting_market(key)),
            |entry| entry.row().clone(),
        );
        match row.market() {
            WatchListMarket::Continuous => continuous.push(row),
            WatchListMarket::Session => session.push(row),
        }
    }
    (continuous.into(), session.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatchListMarket {
    Continuous,
    Session,
}

impl WatchListMarket {
    pub(super) fn of(listing: &VenueListingKey) -> Self {
        let route = &listing.route;
        if route.venue == VenueId::Longbridge && route.scope.as_str() == LONGBRIDGE_US_SCOPE {
            Self::Session
        } else {
            Self::Continuous
        }
    }

    pub(super) fn column_keys(self) -> [&'static str; 4] {
        match self {
            Self::Continuous => [
                "widget.watch_list.change_24h",
                "widget.watch_list.change_percent_24h",
                "widget.watch_list.traded_quantity_24h",
                "widget.watch_list.traded_notional_24h",
            ],
            Self::Session => [
                "widget.watch_list.us.change",
                "widget.watch_list.us.change_percent",
                "widget.watch_list.us.traded_quantity",
                "widget.watch_list.us.traded_notional",
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use ahash::HashMapExt;
    use market_participants_connector::catalog::test_support;
    use market_participants_model::market::{price::MarketPrice, units::QuantityUnit};

    use super::*;

    fn listing(symbol: &str) -> VenueListingKey {
        test_support::listing_key(&test_support::spot_route(), symbol)
    }

    fn stock_listing(symbol: &str) -> VenueListingKey {
        test_support::listing_key(
            &test_support::route("longbridge", LONGBRIDGE_US_SCOPE),
            symbol,
        )
    }

    fn quote() -> AssetId {
        test_support::token("binance", "USDT")
    }

    fn base() -> AssetId {
        test_support::token("binance", "BTC")
    }

    fn price(value: u64) -> MarketPrice {
        MarketPrice {
            value: Price::from(value),
            quote_asset: quote(),
            basis: market_participants_model::listing::venue::PriceBasis::PerAssetUnit,
        }
    }

    fn traded(value: u64) -> MarketQuantity {
        MarketQuantity {
            value: Quantity::from(value),
            unit: QuantityUnit::AssetUnits { asset: base() },
        }
    }

    fn notional(value: u64) -> MarketQuantity {
        MarketQuantity {
            value: Quantity::from(value),
            unit: QuantityUnit::Notional { asset: quote() },
        }
    }

    fn ticker(open: u64, close: u64) -> ListingTicker {
        ListingTicker {
            listing: listing("BTCUSDT"),
            monotonic_id: Some(1),
            open: price(open),
            high: Some(price(close)),
            low: Some(price(close)),
            close: price(close),
            traded_quantity: Some(traded(10)),
            traded_notional: Some(notional(1_000)),
        }
    }

    fn snapshot(ticker: ListingTicker) -> WatchListStreamEvent {
        WatchListStreamEvent::Snapshot {
            ticker: Arc::new(ticker),
            precision: precision(),
        }
    }

    fn price_tick() -> Tick {
        "0.01".parse::<Tick>().expect("a price tick should parse")
    }

    fn precision() -> ListingPrecision {
        ListingPrecision {
            price_tick: price_tick(),
            quantity_tick: "0.001"
                .parse::<Tick>()
                .expect("a quantity step should parse"),
        }
    }

    fn entry(key: &VenueListingKey) -> WatchListEntry {
        WatchListEntry::connecting(key)
    }

    #[test]
    fn format_change_should_use_open_price_as_positive_percentage_denominator() {
        let (_, percentage) = format_change(&ticker(100, 120), price_tick());

        assert_eq!(percentage.to_string(), "20.00%");
    }

    #[test]
    fn format_change_should_preserve_negative_direction() {
        let (change, percentage) = format_change(&ticker(100, 80), price_tick());

        assert_eq!(
            (change.to_string(), percentage.to_string()),
            ("-20.00".to_owned(), "-20.00%".to_owned())
        );
    }

    #[test]
    fn format_change_should_write_the_change_at_the_listings_price_tick() {
        let (change, _) = format_change(&ticker(90_000, 90_010), price_tick());

        assert_eq!(change.to_string(), "10.00");
    }

    #[test]
    fn format_change_should_hide_percentage_when_open_is_zero() {
        let (_, percentage) = format_change(&ticker(0, 10), price_tick());

        assert_eq!(percentage, "--");
    }

    #[test]
    fn format_change_should_report_negative_one_hundred_percent_when_close_is_zero() {
        let (_, percentage) = format_change(&ticker(100, 0), price_tick());

        assert_eq!(percentage, "-100.00%");
    }

    #[test]
    fn format_change_should_round_percentage_to_two_decimal_places() {
        let (_, percentage) = format_change(&ticker(3, 4), price_tick());

        assert_eq!(percentage, "33.33%");
    }

    #[test]
    fn format_volume_should_compact_thousands_with_two_decimals() {
        assert_eq!(format_volume(Quantity::from(1_500_u64)), "1.50K");
    }

    #[test]
    fn a_committed_ticker_should_write_its_price_at_the_listings_tick() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);

        assert!(!entry.apply_event(&snapshot(ticker(90_000, 90_000))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));

        assert_eq!(
            entry
                .row()
                .display()
                .expect("a committed ticker should have labels")
                .price
                .to_string(),
            "90000.00"
        );
    }

    #[test]
    fn sort_entries_should_order_by_the_listing_key() {
        let mut keys = vec![listing("ETHUSDT"), listing("BTCUSDT")];
        sort_entries(&mut keys);

        assert_eq!(keys, vec![listing("BTCUSDT"), listing("ETHUSDT")]);
    }

    #[test]
    fn sort_entries_should_keep_two_listings_of_one_instrument_apart() {
        let first = listing("BTCUSDT");
        let second = listing("BTCUSDTB");
        let mut keys = vec![second.clone(), first.clone()];

        sort_entries(&mut keys);

        let mut expected = vec![first, second];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_ne!(keys[0], keys[1]);
    }

    #[test]
    fn late_loading_connecting_and_error_statuses_should_preserve_committed_ticker() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);
        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        assert!(entry.row().has_ticker());

        entry.apply_event(&WatchListStreamEvent::Status(
            WatchListStatus::LoadingSnapshot,
        ));
        entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Connecting));
        assert!(entry.apply_event(&WatchListStreamEvent::Error(
            WatchListStreamError::MarketParticipant(MarketError::ServiceClosed)
        )));

        assert_eq!(entry.status(), WatchListStatus::Error);
        assert!(
            entry.row().has_ticker(),
            "a failed row keeps the last ticker it accepted"
        );
    }

    #[test]
    fn ready_before_snapshot_should_remain_connecting_until_snapshot() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);

        assert!(entry.apply_event(&WatchListStreamEvent::Status(
            WatchListStatus::LoadingSnapshot
        )));
        assert!(!entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        assert_eq!(entry.status(), WatchListStatus::LoadingSnapshot);
        assert!(!entry.row().has_ticker());

        assert!(entry.apply_event(&snapshot(ticker(100, 120))));
        assert_eq!(entry.status(), WatchListStatus::Ready);
        assert!(entry.row().has_ticker());
    }

    #[test]
    fn snapshot_before_ready_should_be_committed_by_ready() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);

        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        let row = entry.row().clone();
        assert!(!entry.row().has_ticker());
        assert!(Arc::ptr_eq(&row, entry.row()));

        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        assert_eq!(entry.status(), WatchListStatus::Ready);
        assert!(entry.row().has_ticker());
        assert!(!Arc::ptr_eq(&row, entry.row()));
    }

    #[test]
    fn only_the_newest_pending_snapshot_should_survive_until_ready() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);

        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        assert!(!entry.apply_event(&snapshot(ticker(100, 130))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));

        assert_eq!(
            format_change(
                match &entry.ticker {
                    Some(state) => &state.ticker,
                    None => panic!("ready should commit the pending snapshot"),
                },
                price_tick()
            )
            .1
            .to_string(),
            "30.00%"
        );
    }

    #[test]
    fn a_repeated_ready_should_not_be_reported_as_a_change() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);
        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));

        assert!(!entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
    }

    #[test]
    fn an_identical_ticker_should_not_be_reported_as_a_change() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);
        let committed = ticker(100, 120);
        assert!(!entry.apply_event(&snapshot(committed.clone())));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        let row = entry.row().clone();

        assert!(!entry.apply_event(&snapshot(committed)));
        assert!(
            Arc::ptr_eq(&row, entry.row()),
            "an unchanged row must stay shared"
        );
    }

    #[test]
    fn a_new_ticker_should_replace_the_row_and_keep_its_key() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);
        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        let before = entry.row().clone();

        assert!(entry.apply_event(&snapshot(ticker(100, 130))));

        assert!(!Arc::ptr_eq(&before, entry.row()));
        assert!(
            Arc::ptr_eq(before.key(), entry.row().key()),
            "the row keeps naming the same scope"
        );
        assert_eq!(
            entry
                .row()
                .display()
                .expect("a committed ticker should have labels")
                .price
                .to_string(),
            "130.00"
        );
    }

    #[test]
    fn a_row_should_carry_the_units_its_listing_publishes() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);
        let mut only_quantity = ticker(100, 120);
        only_quantity.traded_notional = None;
        assert!(!entry.apply_event(&snapshot(only_quantity)));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));

        let display = entry.row().display().expect("the row has labels");

        assert_eq!(display.traded_quantity, Some(traded(10)));
        assert_eq!(
            display.traded_notional, None,
            "a missing unit is missing, not zero and not the other unit"
        );
    }

    #[test]
    fn error_should_latch_against_late_snapshots_and_statuses() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);
        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        let committed = entry.row().clone();

        assert!(entry.apply_event(&WatchListStreamEvent::Error(
            WatchListStreamError::MarketParticipant(MarketError::ServiceClosed)
        )));
        assert!(!entry.apply_event(&snapshot(ticker(100, 999))));
        assert!(!entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));

        assert_eq!(entry.status(), WatchListStatus::Error);
        assert!(!Arc::ptr_eq(&committed, entry.row()));
        assert_eq!(
            entry
                .row()
                .display()
                .expect("the failed row keeps its last ticker")
                .price
                .to_string(),
            "120.00"
        );
    }

    #[test]
    fn begin_retry_should_keep_the_last_ticker_and_wait_for_a_new_snapshot() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);
        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        assert!(entry.apply_event(&WatchListStreamEvent::Error(
            WatchListStreamError::MarketParticipant(MarketError::ServiceClosed)
        )));

        assert!(entry.begin_retry());
        assert_eq!(entry.status(), WatchListStatus::Connecting);
        assert!(entry.error().is_none());
        assert!(
            entry.row().has_ticker(),
            "the row keeps showing what it had"
        );

        assert!(!entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        assert_eq!(
            entry.status(),
            WatchListStatus::Connecting,
            "a new stream is ready only once it has a snapshot"
        );

        assert!(entry.apply_event(&snapshot(ticker(100, 130))));
        assert_eq!(entry.status(), WatchListStatus::Ready);
    }

    #[test]
    fn row_display_should_initialize_lazily_once() {
        let key = listing("BTCUSDT");
        let mut entry = entry(&key);
        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        let row = entry.row().clone();

        let first = row
            .display()
            .expect("a committed ticker should have labels");
        let second = row.display().expect("the row keeps its labels");

        assert!(std::ptr::addr_eq(first, second));
    }

    #[test]
    fn market_of_should_read_the_scope_a_listing_is_quoted_on() {
        assert_eq!(
            WatchListMarket::of(&listing("BTCUSDT")),
            WatchListMarket::Continuous,
            "a venue that trades around the clock has no session to head its table"
        );
        assert_eq!(
            WatchListMarket::of(&stock_listing("AAPL")),
            WatchListMarket::Session,
            "a market that keeps a calendar is the other table"
        );
    }

    #[test]
    fn market_of_should_answer_for_a_listing_no_generation_holds() {
        let stock = stock_listing("AAPL");
        let crypto = listing("BTCUSDT");

        let (continuous, session) = split_rows(&[stock.clone(), crypto.clone()], &HashMap::new());

        assert_eq!(
            continuous
                .iter()
                .map(|row| row.key().as_ref().clone())
                .collect::<Vec<_>>(),
            vec![crypto],
            "a listing with no market behind it is still placed by its own route"
        );
        assert_eq!(
            session
                .iter()
                .map(|row| row.key().as_ref().clone())
                .collect::<Vec<_>>(),
            vec![stock]
        );
        assert!(
            !continuous[0].has_ticker() && !session[0].has_ticker(),
            "a row with no market behind it carries no values"
        );
    }

    #[test]
    fn split_rows_should_keep_the_configured_order_and_share_unchanged_rows() {
        let keys = vec![listing("BTCUSDT"), listing("ETHUSDT")];
        let session = stock_listing("AAPL");
        let mut all = keys.clone();
        all.push(session);
        let mut entries = HashMap::with_capacity(all.len());
        for key in &all {
            entries.insert(key.clone(), entry(key));
        }

        let (continuous, sessions) = split_rows(&all, &entries);
        assert!(!Arc::ptr_eq(&continuous, &sessions));

        let entry = entries.get_mut(&keys[0]).expect("BTC should exist");
        assert!(!entry.apply_event(&snapshot(ticker(100, 120))));
        assert!(entry.apply_event(&WatchListStreamEvent::Status(WatchListStatus::Ready)));
        let (after_continuous, after_sessions) = split_rows(&all, &entries);

        assert!(!Arc::ptr_eq(&continuous, &after_continuous));
        assert!(
            Arc::ptr_eq(&continuous[1], &after_continuous[1]),
            "a row that did not change is shared rather than rebuilt"
        );
        assert!(
            Arc::ptr_eq(&sessions[0], &after_sessions[0]),
            "and a table nothing moved in keeps every row it had"
        );
        assert_eq!(
            after_continuous
                .iter()
                .chain(after_sessions.iter())
                .map(|row| row.key().as_ref().clone())
                .collect::<Vec<_>>(),
            all,
            "the two tables hold every followed listing once, in configured order"
        );
    }
}
