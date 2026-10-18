//! What one watch list holds, and what each followed listing is doing.
//!
//! A watch list follows up to [`MAX_WATCH_LIST_ENTRIES`] listings at once, one
//! stream each. Everything that arrives for a listing goes through its
//! [`WatchListEntry`], which is the only place that decides what a stream event
//! means: the entry keeps the last accepted ticker, the status the stream
//! reported, and the failure that ended it.
//!
//! # Ready is a boundary, not "the first snapshot arrived"
//!
//! The reliable channel and the latest-value channel are independent, so a
//! `Ready` status and the snapshot it refers to can arrive in either order. An
//! entry therefore holds a snapshot that arrived *before* `Ready` as pending and
//! commits it when `Ready` arrives, and remembers `Ready` as received and commits
//! the next snapshot. Either way a row is only ever reported ready with a ticker
//! that belongs to the stream generation that reported it.
//!
//! # A row is immutable
//!
//! Formatting is not free: every price, change and traded value is turned into
//! shared text. So an entry builds its [`WatchListRow`] once per accepted change
//! and keeps it, the row builds its [`WatchListRowDisplay`] the first time it is
//! drawn, and a row that did not change is handed to the table again as the same
//! `Arc` rather than rebuilt.
//!
//! # Values are stored, units are attached while drawing
//!
//! A ticker carries each value together with the unit it is measured in: a traded
//! quantity is counted in the listing's own traded unit and a traded notional in
//! the asset the venue reports the amount in. Neither is a property of the row, so
//! the row keeps the values with their units and the unit labels are resolved where
//! the row is drawn.
//!
//! # Precision travels with the ticker
//!
//! The increments a number is written at belong to the listing and not to the row,
//! so they are resolved once with the stream that carries the listing's tickers and
//! kept beside the ticker they describe. A row is therefore written at the precision
//! of the stream generation that published the snapshot it shows, and a price of
//! `90000` on a listing with a tick of `0.01` reads as `90000.00` rather than as the
//! canonical form that drops the zeroes.
//!
//! # A row outlives the market behind it
//!
//! The listings of a market the user has not loaded are still the user's list, so
//! their rows are still drawn: what the widget follows is a configuration, and the
//! market behind it comes and goes. Such a row says which market participant has to
//! be loaded instead of carrying values, and it is filled as soon as that market
//! commits. Which of the two tables it is drawn in is therefore read off the
//! listing's own route rather than off the directory: the scope a listing is quoted
//! on is what decides whether its venue keeps a session, and a directory is not
//! consulted for a fact that has to be known before one is loaded.

use std::{
    cmp::Ordering,
    sync::{Arc, OnceLock},
};

use ahash::HashMap;
use gpui_kit::SharedString;
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

/// The most listings one watch list follows.
pub(super) const MAX_WATCH_LIST_ENTRIES: usize = 100;

/// What one followed listing is doing.
///
/// A stream publishes `Connecting`, `LoadingSnapshot` and `Ready`. `Error` is the
/// state an entry settles into once its stream has failed; it is never published,
/// because a failure carries its reason in [`WatchListStreamEvent::Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatchListStatus {
    Connecting,
    LoadingSnapshot,
    Ready,
    Error,
}

/// Why one listing's stream stopped.
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

/// One message published by one listing's stream.
#[derive(Clone, Debug)]
pub(super) enum WatchListStreamEvent {
    Status(WatchListStatus),
    Snapshot {
        ticker: Arc<ListingTicker>,
        /// The increments of the listing the ticker belongs to, read from the same
        /// resolution that opened the stream.
        precision: ListingPrecision,
    },
    Error(WatchListStreamError),
}

/// One accepted ticker, with the increments of the listing that published it.
///
/// The two always travel together: a price is only readable at the precision of
/// the listing it was quoted on, so a ticker kept without one could not be drawn. It is
/// named outside this module because a row's ticker is part of what a row says it holds.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct TickerState {
    ticker: Arc<ListingTicker>,
    precision: ListingPrecision,
}

/// Everything the widget knows about one followed listing.
#[derive(Debug)]
pub(super) struct WatchListEntry {
    status: WatchListStatus,
    ticker: Option<TickerState>,
    /// A snapshot that arrived before the `Ready` status that refers to it.
    pending_snapshot: Option<TickerState>,
    ready_received: bool,
    error: Option<WatchListStreamError>,
    row: Arc<WatchListRow>,
}

impl WatchListEntry {
    /// Creates the state a followed listing starts in: connecting, with nothing
    /// to draw yet.
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

    /// Applies one stream event, reporting whether the drawn row changed.
    pub(super) fn apply_event(&mut self, event: &WatchListStreamEvent) -> bool {
        match event {
            WatchListStreamEvent::Status(status) => self.apply_status(*status),
            WatchListStreamEvent::Snapshot { ticker, precision } => {
                self.apply_snapshot(ticker.clone(), *precision)
            }
            WatchListStreamEvent::Error(error) => self.apply_error(error),
        }
    }

    /// Restarts this entry for the stream generation a retry opened.
    ///
    /// The last ticker is kept: the user asked for the stream to be reopened, not
    /// for the prices the row is showing to be forgotten.
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
            // An entry starts connecting, and a stream that reconnects re-reports
            // it without changing anything a row draws.
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
                // `Ready` may have overtaken the snapshot it refers to. The entry
                // then waits for the next snapshot instead of reporting a ready
                // row that still has nothing to show.
                let Some(ticker) = self.pending_snapshot.take() else {
                    return false;
                };
                self.commit(ticker)
            }
            // A failure is published as an event, which carries its reason.
            WatchListStatus::Error => false,
        }
    }

    fn apply_snapshot(&mut self, ticker: Arc<ListingTicker>, precision: ListingPrecision) -> bool {
        // A failed entry keeps the ticker it has and ignores everything else: the
        // stream that produced this snapshot is not the one the user is looking at
        // any more, and only an explicit retry may open a new one.
        if self.status == WatchListStatus::Error {
            return false;
        }
        let state = TickerState { ticker, precision };
        if !self.ready_received {
            // The latest-value channel only ever hands over the newest snapshot of
            // a key, so keeping the last pending one is keeping the newest.
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

/// One followed listing, as the table draws it.
///
/// A row is immutable. An entry replaces its row whenever the entry changes, so
/// an unchanged row can be handed to the table again as the same `Arc`, and the
/// text of a row is formatted at most once.
#[derive(Debug)]
pub(super) struct WatchListRow {
    key: Arc<VenueListingKey>,
    /// Which of the two tables this row is drawn in.
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

    /// The row of a listing whose market is not in memory.
    ///
    /// The listing is still followed — it is the user's own list — but nothing about
    /// it can be read until the market participant's metadata is loaded, so the row
    /// carries the venue that has to be loaded instead of a stream status.
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

    /// Which table this row belongs to, read off its own route.
    pub(super) fn market(&self) -> WatchListMarket {
        self.market
    }

    pub(super) fn state(&self) -> &WatchListRowState {
        &self.state
    }

    /// The stream status of this row, which a row waiting for its market has none of.
    pub(super) fn status(&self) -> WatchListStatus {
        match &self.state {
            WatchListRowState::AwaitingMarket => WatchListStatus::Connecting,
            WatchListRowState::Ticker { status, .. } => *status,
        }
    }

    pub(super) fn has_ticker(&self) -> bool {
        matches!(&self.state, WatchListRowState::Ticker { ticker, .. } if ticker.is_some())
    }

    /// The labels of this row, built the first time the row is drawn.
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

/// What one followed listing is doing, and how the row says it.
///
/// The two are mutually exclusive, which is the whole point of the sum: a listing
/// the directory cannot answer for is not a listing whose stream is connecting, and
/// a row that carried both would say two things about one listing.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum WatchListRowState {
    /// The market participant this listing is on is not loaded, so there is nothing
    /// to draw but the name of the market that has to be loaded.
    AwaitingMarket,
    /// The listing is read: its stream is at `status`, and it carries the last
    /// accepted ticker once it has one.
    Ticker {
        status: WatchListStatus,
        ticker: Option<TickerState>,
    },
}

impl WatchListRowState {
    /// Whether this row is backed by a stream.
    ///
    /// What a row is *doing* is not the same question: a row that failed before its first
    /// ticker is still a row with a stream, and reopening that stream is a thing the user
    /// can do, while a row waiting for a market has no stream at all.
    pub(super) const fn is_read(&self) -> bool {
        matches!(self, Self::Ticker { .. })
    }
}

/// The values one row draws, each still carrying its own unit.
#[derive(Debug)]
pub(super) struct WatchListRowDisplay {
    pub price: SharedString,
    pub quote_asset: AssetId,
    pub change: SharedString,
    pub change_percent: SharedString,
    /// The traded quantity of the session, in the unit the listing publishes it in.
    pub traded_quantity: Option<MarketQuantity>,
    /// The traded notional of the session, in the asset the venue states it in.
    pub traded_notional: Option<MarketQuantity>,
    pub direction: PriceDirection,
}

impl WatchListRowDisplay {
    fn new(ticker: &ListingTicker, precision: ListingPrecision) -> Self {
        let (change, change_percent) = format_change(ticker, precision.price_tick);

        Self {
            price: SharedString::new(
                ticker
                    .close
                    .value
                    .format_and_round_tick(precision.price_tick)
                    .as_str(),
            ),
            quote_asset: ticker.close.quote_asset.clone(),
            change,
            change_percent,
            // A listing that publishes no traded quantity, or no traded notional,
            // has nothing for that column: neither zero nor the other unit may be
            // shown in its place.
            traded_quantity: ticker.traded_quantity.clone(),
            traded_notional: ticker.traded_notional.clone(),
            direction: PriceDirection::of(ticker),
        }
    }
}

/// Which way the listing moved over the period the ticker covers.
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

/// The absolute and relative change from the listing's open to its close.
///
/// The change is measured away from the open price and its direction is carried
/// separately, so a price that fell reads as the fall it was rather than as a
/// difference that lost its sign. It is written at the listing's price tick, like
/// the price it is measured from. The percentage is hidden when the open price is
/// zero, because there is no denominator to measure against.
pub(super) fn format_change(
    ticker: &ListingTicker,
    price_tick: Tick,
) -> (SharedString, SharedString) {
    let current = ticker.close.value;
    let open = ticker.open.value;
    let (magnitude, is_negative) = if current >= open {
        (current.checked_sub(open).unwrap_or(Price::ZERO), false)
    } else {
        (open.checked_sub(current).unwrap_or(Price::ZERO), true)
    };
    let magnitude_text = magnitude.format_and_round_tick(price_tick);
    let change = if is_negative {
        SharedString::new(format_smolstr!("-{magnitude_text}").as_str())
    } else {
        SharedString::new(magnitude_text.as_str())
    };

    let change_percent = if open == Price::ZERO {
        SharedString::new_static("--")
    } else {
        let magnitude_percent = magnitude.inner() as f64 / open.inner() as f64 * 100.0;
        let percent = if is_negative {
            -magnitude_percent
        } else {
            magnitude_percent
        };
        SharedString::new(format_smolstr!("{percent:.2}%").as_str())
    };

    (change, change_percent)
}

/// Compacts a traded value into a scale a column can hold.
///
/// A 24-hour traded value is routinely eighteen digits wide, which no column shows
/// usefully, so it is scaled down to millions or thousands with two decimals.
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

/// Orders the followed listings by their canonical key.
///
/// The order is the displayed order, so it is settled here rather than by the
/// table: two lists that follow the same listings read the same way, whatever
/// order the user added them in.
pub(super) fn sort_entries(entries: &mut [VenueListingKey]) {
    entries.sort_unstable();
}

/// Splits the followed listings into the two markets, in configured order.
///
/// Returns `(continuous, session)`, in that order. The rows of the listings the widget
/// holds an entry for are handed over as they are, and every other followed listing is
/// drawn as a row waiting for its market: a listing whose market participant is not
/// loaded yet is still a listing the user follows, so it is drawn rather than skipped —
/// which is what a list opened before its market is loaded consists of. A row that did
/// not change is handed back as the same `Arc`, which is what lets a table see that
/// nothing it draws has moved.
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

/// The two markets one watch list draws: the ones that trade around the clock, and the
/// ones whose market keeps a calendar.
///
/// The two are the two shapes of statistics this build can state, not two directories: a
/// session's numbers belong to a session that closes and an around-the-clock market's roll
/// over a day, so the two never share a heading. Which one a listing belongs to is read off
/// the scope it is quoted on, which is part of its own key — see [`WatchListMarket::of`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WatchListMarket {
    /// The venues that trade around the clock.
    Continuous,
    /// The venues whose session keeps a calendar, which today is one US-equity market.
    Session,
}

impl WatchListMarket {
    /// Which table one listing is drawn in.
    ///
    /// The route a listing is quoted on decides it, and both halves of the route are read: the
    /// scope this build carries for a session market is Longbridge's US-equity one, and a scope
    /// is only ever one venue's — a route on another venue that happened to use the same scope
    /// name is not that market, and drawing it under the session headings would be a table
    /// saying session statistics over rows no session market quotes. Every other route is a
    /// market that trades around the clock. The route is part of the listing's own key, so this
    /// is an answer about the listing rather than about the directory — which is what makes a
    /// row drawable before the market behind it is loaded. A route the build does not carry
    /// would be one no user could have stored, and it is read as the continuous market rather
    /// than as a table nothing heads.
    pub(super) fn of(listing: &VenueListingKey) -> Self {
        let route = &listing.route;
        if route.venue == VenueId::Longbridge && route.scope.as_str() == LONGBRIDGE_US_SCOPE {
            Self::Session
        } else {
            Self::Continuous
        }
    }

    /// The four column labels of this market, in column order: change, change percent,
    /// traded quantity, traded notional.
    ///
    /// The labels are keys rather than text, because the language a table is drawn in is
    /// the frame's and not the table's. The two sets say different things about the
    /// period they cover — one market's statistics roll over a day, the other's belong to
    /// a session that closes — so neither set may be shown under the other's heading.
    /// They are read whether or not the market behind the table is loaded: a row waiting
    /// for its market is drawn under the headings of the market it belongs to, and the
    /// merged cell below them says what is missing rather than the headings lying.
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

    /// The same, on the one scope this build quotes a session market on.
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

    /// A price tick of one cent and a quantity step of one thousandth, so a label
    /// that keeps the listing's precision is told apart from the canonical form.
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

    /// The change is written at the listing's own tick, so a difference that is a
    /// whole number of coins keeps the decimals the tick states.
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

    /// A price is written at the price tick of the listing it was quoted on, so a
    /// whole price keeps the decimals the tick states instead of being read as the
    /// canonical form that drops them.
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

    /// Two listings of one instrument are two scopes, and the order they are
    /// followed in is the order of their keys rather than of the instrument.
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

    /// The row keeps each traded value with the unit the ticker published it in, and a
    /// unit the listing does not publish stays missing instead of becoming the other
    /// one.
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

    /// The scope a listing is quoted on decides which table it is drawn in, and the two
    /// answers are the two kinds of scope this build carries.
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

    /// The partition reads the listing's own route rather than the directory, which is what
    /// lets it place a listing whose market participant is not loaded — the state this
    /// build has no calendar for at all.
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

    /// The two tables together hold every followed listing exactly once, in the order the
    /// configuration states, and a row that did not change is still the same `Arc`.
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
