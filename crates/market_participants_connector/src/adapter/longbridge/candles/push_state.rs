//! The state a subscription keeps between the venue's pushes and its candles.
//!
//! Two things live here that are neither the wire nor the aggregation. The first is the
//! market's session table, which a push is bucketed by: an ordinary US day has one, and a
//! half day has a different one, so the table a push is read against depends on the dates
//! the venue published as half days (cmd 9). The second is that list of half days itself,
//! which is read lazily — nothing before the first push has needed it — and refreshed
//! once a day after that.
//!
//! The third thing here is which pushes become prints at all, and that is where the
//! venue's third gate lives: a **quote** is merged only while the regular session is open
//! (§4.6 step 0), so a pre-market, post-market or overnight quote builds no candle of any
//! interval. The aggregation states the other two gates, because they are about where a
//! print's bucket starts rather than about whether the print exists.
//!
//! A read that fails is not an error a reader sees: the venue's own client treats an
//! unreadable calendar as an ordinary day, and so does this one, with a `log::warn!` as
//! the only trace. The state that owns the tables is a plain value with no socket in it,
//! which is what lets the aggregation be tested by installing a table and pushing frames
//! at it.

use longbridge_proto::quote::{PushQuote, PushTrade};
use market_participants_model::decimal::{Price, Quantity, UnixMillis};

use super::{
    super::codec::LongbridgeSession,
    aggregator::{CandlePrint, CumulativeDay, QuotePrint, TradePrint, TradeType},
    session_table::{
        HALF_DAY_SESSION_TABLE, MARKET_ZONE, MarketSession, NORMAL_SESSION_TABLE, SessionTable,
    },
};
use crate::{error::MarketParticipantError, market::timeframe::TimeFrame};

/// How long one list of half days is trusted before it is read again (§4.6).
pub(super) const HALF_DAYS_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The operation a field of a push that cannot be read is reported as.
const DECODE_PUSH: &str = "decode the Longbridge push";

/// One subscription's push-path state.
pub(super) struct PushState {
    /// The interval the candles are cut into.
    timeframe: TimeFrame,
    /// The zone the venue opens its candles in.
    zone: jiff::tz::TimeZone,
    /// The half days the venue last published, with the instant to read them again.
    half_days: Option<HalfDays>,
}

/// The half days one read of cmd 9 produced.
struct HalfDays {
    /// The market-local midnights of the half days, which is what a bucket is compared
    /// against.
    midnights: ahash::AHashSet<UnixMillis>,
    /// The earliest instant the list is worth reading again.
    due: std::time::Instant,
}

impl PushState {
    /// Opens the push state of one subscription.
    pub(super) fn new(timeframe: TimeFrame) -> Self {
        Self {
            timeframe,
            // The zone is the market's own and the database ships with `jiff`, so this
            // cannot fail on a machine without a system time-zone database; UTC is the
            // fallback the type has, and the market's own rules are the ones every
            // assertion here is made against.
            zone: jiff::tz::TimeZone::get(MARKET_ZONE).unwrap_or(jiff::tz::TimeZone::UTC),
            half_days: None,
        }
    }

    /// Reports whether the trading-day table is due to be read.
    ///
    /// `true` means it has never been read, or the day it was read on has passed; the
    /// caller reads it and installs the answer with [`Self::install_calendar`].
    pub(super) fn half_days_due(&self, now: std::time::Instant) -> bool {
        self.half_days
            .as_ref()
            .is_none_or(|half_days| now >= half_days.due)
    }

    /// Installs what one read of the venue's calendar found, and when to read it again.
    ///
    /// The values are the venue's own `half_trade_day` dates, and one that is not a date
    /// is dropped with a warning rather than guessed at: a calendar this build cannot
    /// read is a calendar it does not have, and the tables below then fall back to the
    /// ordinary day — which is the same fail-open the venue's own client does.
    ///
    /// An empty list is therefore a statement rather than a missing value, and it is the
    /// one a read that failed leaves: the table that follows is the ordinary one, and the
    /// deadline that follows is a day away, so a calendar the venue would not state is
    /// asked about once a day rather than once per push.
    pub(super) fn install_calendar(&mut self, days: &[String], now: std::time::Instant) {
        let mut midnights = ahash::AHashSet::with_capacity(days.len());
        for day in days {
            let Some(date) = parse_day(day) else {
                log::warn!("the Longbridge trading days name `{day}`, which is not a date");
                continue;
            };
            midnights.insert(midnight_of(date, &self.zone));
        }
        log::debug!(
            "the Longbridge trading days state {} half days in the window",
            midnights.len()
        );
        self.half_days = Some(HalfDays {
            midnights,
            due: now + HALF_DAYS_TTL,
        });
    }

    /// Returns the market-local midnights of the half days the state holds, which the
    /// tests read back to state what one read of the calendar installed.
    #[cfg(test)]
    pub(super) fn half_days_midnights(&self) -> ahash::AHashSet<UnixMillis> {
        self.half_days
            .as_ref()
            .map(|half_days| half_days.midnights.clone())
            .unwrap_or_default()
    }

    /// Returns the session table one instant's candle is bucketed by.
    ///
    /// A half day's table replaces the whole ordinary one rather than shortening one
    /// session of it: its post session opens where its regular session closes, so every
    /// afternoon candle would otherwise be cut on the wrong grid.
    pub(super) fn session_table(&self, millis: UnixMillis) -> &'static SessionTable {
        let Some(half_days) = &self.half_days else {
            return &NORMAL_SESSION_TABLE;
        };
        if half_days
            .midnights
            .contains(&midnight_of_millis(millis, &self.zone))
        {
            &HALF_DAY_SESSION_TABLE
        } else {
            &NORMAL_SESSION_TABLE
        }
    }

    /// Returns the market-local day one instant fell in, as the venue formats it.
    ///
    /// The trading-day window is asked about around *this* day rather than around the
    /// machine's: the venue's calendar is the market's, and a reader whose machine is a
    /// day ahead would otherwise ask about a window that starts after the session it is
    /// subscribing to.
    pub(super) fn market_day(&self, millis: UnixMillis) -> Option<jiff::civil::Date> {
        jiff::Timestamp::from_millisecond(millis)
            .ok()
            .map(|timestamp| jiff::Zoned::new(timestamp, self.zone.clone()).date())
    }

    /// Reads one quote push as the print it carries, when it carries one.
    ///
    /// A quote of an extended session contributes nothing, which is §4.6's third gate:
    /// the venue merges quotes only while the regular session is open, so a pre-market or
    /// overnight quote is not a price of the candle being built.
    ///
    /// Of the regular session's quotes, only `Day1` is written from the venue's cumulative
    /// counters: §4.6 step 1 states the quote merge for `DAY` and the regular session, and
    /// every other interval — a week, a month, a quarter, a year, an hour — accumulates
    /// the increment its counters advanced by, because a day's counters are not a week's
    /// candle. A quote that advanced nothing still states a price, so it is merged as a
    /// price with no volume rather than dropped; a quote that carried nothing at all —
    /// no counter, no increment — is the venue repeating the state a reader already has
    /// and builds nothing.
    ///
    /// The frame's own tag is not read here: an end-of-day quote (§4.6) is merged exactly
    /// as an ordinary one, because the venue merges it before its own end-of-day
    /// judgement, and the difference the tag makes is that the frame is not forwarded as
    /// a ticker — which is a surface this build does not serve.
    pub(super) fn quote_print(
        &self,
        quote: &PushQuote,
    ) -> Result<Option<CandlePrint>, MarketParticipantError> {
        let session = market_session(quote.trade_session)?;
        if !session.is_normal() {
            return Ok(None);
        }
        let timestamp = seconds_to_millis(quote.timestamp)?;
        let price = decimal(&quote.last_done)?;
        let cumulative = match self.timeframe {
            TimeFrame::Day1 => Some(CumulativeDay {
                open: decimal(&quote.open)?,
                high: decimal(&quote.high)?,
                low: decimal(&quote.low)?,
                close: price,
                volume: volume_of(quote.volume)?,
            }),
            _ => None,
        };
        let volume_delta = match u64::try_from(quote.current_volume) {
            Ok(0) | Err(_) if cumulative.is_none() => None,
            Ok(volume) => Some(Quantity::from(volume)),
            // A negative increment cannot be added to a candle, and a day's candle does
            // not need it: the counters above are the whole of what it states.
            Err(_) => None,
        };
        Ok(Some(CandlePrint::quote(QuotePrint {
            timestamp,
            session,
            price,
            volume_delta,
            cumulative,
        })))
    }

    /// Reads one trade push as the prints it carries.
    ///
    /// Every entry of the frame is one print of the venue's tape, with its own price,
    /// volume, session and type. A trade states no notional of its own, so the amount a
    /// candle accumulates is the print's `price × volume × lot_size` with a US share's
    /// own lot size — one (§4.6).
    pub(super) fn trade_prints(
        &self,
        trades: &PushTrade,
    ) -> Result<Vec<CandlePrint>, MarketParticipantError> {
        let mut prints = Vec::with_capacity(trades.trade.len());
        for trade in &trades.trade {
            prints.push(CandlePrint::trade(TradePrint {
                timestamp: seconds_to_millis(trade.timestamp)?,
                session: market_session(trade.trade_session)?,
                price: decimal(&trade.price)?,
                volume: volume_of(trade.volume)?,
                trade_type: named_type(&trade.trade_type),
            }));
        }
        Ok(prints)
    }
}

/// Reads one wire timestamp as milliseconds.
///
/// Every timestamp the venue pushes is Unix **seconds** — the candle path says the same
/// thing where it decodes history — so a value read as milliseconds would place every
/// candle in 1970.
fn seconds_to_millis(seconds: i64) -> Result<UnixMillis, MarketParticipantError> {
    seconds
        .checked_mul(1_000)
        .ok_or_else(|| MarketParticipantError::ProtocolError {
            operation: smol_str::SmolStr::new_static(DECODE_PUSH),
            reason: smol_str::format_smolstr!("a timestamp of {seconds} seconds overflows"),
            raw_data: None,
        })
}

/// Reads one wire decimal as a price.
fn decimal(value: &str) -> Result<Price, MarketParticipantError> {
    value
        .parse()
        .map_err(|_| MarketParticipantError::ProtocolError {
            operation: smol_str::SmolStr::new_static(DECODE_PUSH),
            reason: smol_str::format_smolstr!("`{value}` is not a decimal"),
            raw_data: None,
        })
}

/// Reads one wire volume as a quantity.
fn volume_of(volume: i64) -> Result<Quantity, MarketParticipantError> {
    u64::try_from(volume)
        .map(Quantity::from)
        .map_err(|_| MarketParticipantError::ProtocolError {
            operation: smol_str::SmolStr::new_static(DECODE_PUSH),
            reason: smol_str::format_smolstr!("a volume of {volume} is not a quantity"),
            raw_data: None,
        })
}

/// Converts one wire session to the aggregator's own vocabulary.
fn market_session(number: i32) -> Result<MarketSession, MarketParticipantError> {
    let Some(session) = LongbridgeSession::of(number) else {
        return Err(MarketParticipantError::ProtocolError {
            operation: smol_str::SmolStr::new_static(DECODE_PUSH),
            reason: smol_str::format_smolstr!("the venue filed the print under session {number}"),
            raw_data: None,
        });
    };
    Ok(match session {
        LongbridgeSession::Normal => MarketSession::Normal,
        LongbridgeSession::Pre => MarketSession::Pre,
        LongbridgeSession::Post => MarketSession::Post,
        LongbridgeSession::Overnight => MarketSession::Overnight,
    })
}

/// Reads one wire trade type as the table's own vocabulary.
///
/// The venue's ordinary case is an empty string, which is the type the table names with
/// its `""` row rather than a missing value: "the venue named no type" and "the venue
/// named a type this build does not know" are two different rows, and both are read here
/// rather than at the table, so the table has one spelling per type.
fn named_type(trade_type: &str) -> TradeType {
    TradeType::of(trade_type)
}

/// Reads one `YYYYMMDD` as a date.
///
/// The text is the venue's own and arrives off the socket, so its digits are judged
/// before they are read: slicing a string by byte offsets panics on any input whose
/// characters are not all one byte wide — `"123é456"` is eight bytes long and its fourth
/// byte is inside a two-byte character — and a calendar the venue worded differently must
/// be an ordinary day rather than a panic.
fn parse_day(text: &str) -> Option<jiff::civil::Date> {
    jiff::civil::Date::new(
        i16::try_from(digits(text, 0..4)?).ok()?,
        i8::try_from(digits(text, 4..6)?).ok()?,
        i8::try_from(digits(text, 6..8)?).ok()?,
    )
    .ok()
}

/// Reads one ASCII run of digits as its number.
fn digits(text: &str, range: std::ops::Range<usize>) -> Option<i32> {
    let bytes = text.as_bytes().get(range)?;
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// The instant one market-local date's midnight is, in milliseconds.
fn midnight_of(date: jiff::civil::Date, zone: &jiff::tz::TimeZone) -> UnixMillis {
    date.at(0, 0, 0, 0)
        .to_zoned(zone.clone())
        .map(|midnight| midnight.timestamp().as_millisecond())
        .unwrap_or(UnixMillis::MIN)
}

/// The market-local midnight of the day one instant fell in.
fn midnight_of_millis(millis: UnixMillis, zone: &jiff::tz::TimeZone) -> UnixMillis {
    jiff::Timestamp::from_millisecond(millis)
        .ok()
        .map(|timestamp| midnight_of(jiff::Zoned::new(timestamp, zone.clone()).date(), zone))
        .unwrap_or(UnixMillis::MIN)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use longbridge_proto::quote::{PushQuote, PushTrade, Trade};
    use market_participants_model::decimal::{Price, Quantity, UnixMillis};

    use super::{HALF_DAYS_TTL, MarketSession, PushState, midnight_of};
    use crate::{
        adapter::longbridge::candles::session_table::{
            HALF_DAY_SESSION_TABLE, MARKET_ZONE, NORMAL_SESSION_TABLE,
        },
        market::timeframe::TimeFrame,
    };

    /// One market-local wall time as milliseconds since the epoch.
    fn at(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> UnixMillis {
        jiff::civil::date(year, month, day)
            .at(hour, minute, 0, 0)
            .to_zoned(jiff::tz::TimeZone::get(MARKET_ZONE).expect("the market zone is in the db"))
            .expect("the fixture wall time is a real instant")
            .timestamp()
            .as_millisecond()
    }

    /// One push state of one subscription, with no calendar read yet.
    fn state() -> PushState {
        PushState::new(TimeFrame::Hour1)
    }

    /// One quote of one session, advancing the venue's own counter by one share.
    fn quote(session: i32, seconds: i64) -> PushQuote {
        PushQuote {
            symbol: "AAPL.US".to_owned(),
            last_done: "101.00".to_owned(),
            open: "100.00".to_owned(),
            high: "101.00".to_owned(),
            low: "100.00".to_owned(),
            timestamp: seconds,
            volume: 11,
            current_volume: 1,
            current_turnover: "101.00".to_owned(),
            trade_status: 0,
            trade_session: session,
            sequence: 1,
            turnover: String::new(),
            tag: 0,
        }
    }

    /// The half-day table is read lazily and then trusted for a day: the state says it is
    /// due before anything has been read, and again once the day has passed. A read that
    /// found nothing — the one a failed read leaves — is trusted for the same day, which
    /// is what keeps a venue that will not state its calendar from being asked once per
    /// push.
    #[test]
    fn the_half_days_are_read_lazily_and_refreshed_daily() {
        let now = Instant::now();
        let mut state = state();
        assert!(
            state.half_days_due(now),
            "nothing has been read, so the calendar is due"
        );

        state.install_calendar(&["20261127".to_owned()], now);
        assert!(
            !state.half_days_due(now + HALF_DAYS_TTL - Duration::from_secs(1)),
            "and it is trusted for the day it was read on"
        );
        assert!(
            state.half_days_due(now + HALF_DAYS_TTL),
            "until the day has passed"
        );

        state.install_calendar(&[], now);
        assert!(
            !state.half_days_due(now + HALF_DAYS_TTL - Duration::from_secs(1)),
            "a read that stated no half day is a read that happened"
        );
        assert_eq!(
            state
                .session_table(at(2026, 11, 27, 16, 0))
                .session(MarketSession::Normal),
            NORMAL_SESSION_TABLE.session(MarketSession::Normal),
            "and the ordinary table is what it leaves in force"
        );
    }

    /// A half day replaces the table of its own market day and of no other, and a value
    /// that is not a date is dropped rather than guessed at.
    #[test]
    fn a_half_day_replaces_the_table_of_its_own_market_day() {
        let mut state = state();
        state.install_calendar(
            &["20261127".to_owned(), "not-a-date".to_owned()],
            Instant::now(),
        );

        assert_eq!(
            state
                .session_table(at(2026, 11, 27, 16, 0))
                .session(MarketSession::Normal),
            HALF_DAY_SESSION_TABLE.session(MarketSession::Normal),
            "the half day's own regular session ends at 13:00"
        );
        assert_eq!(
            state
                .session_table(at(2026, 11, 26, 16, 0))
                .session(MarketSession::Normal),
            NORMAL_SESSION_TABLE.session(MarketSession::Normal),
            "and the day before it is an ordinary one"
        );
        assert_eq!(
            state
                .session_table(at(2026, 11, 26, 21, 0))
                .session(MarketSession::Normal),
            NORMAL_SESSION_TABLE.session(MarketSession::Normal),
            "the evening before the half day is still its own market day"
        );
    }

    /// A date the venue worded with a character wider than one byte is not a date rather
    /// than a panic: `"123é456"` is eight bytes long and its fourth byte is the middle of
    /// a two-byte character, which is exactly the input a byte-offset slice would break
    /// on. The calendar is the venue's own text, so every value of it has to be readable
    /// as "not a date" without taking the reader down.
    #[test]
    fn a_calendar_value_that_is_not_ascii_digits_is_not_a_date() {
        let mut state = state();
        state.install_calendar(
            &[
                "123é456".to_owned(),
                "2026-1127".to_owned(),
                "２０２６１１２７".to_owned(),
                "2026112a".to_owned(),
                "202611".to_owned(),
                "20261127".to_owned(),
            ],
            Instant::now(),
        );

        assert_eq!(
            state
                .session_table(at(2026, 11, 27, 16, 0))
                .session(MarketSession::Normal),
            HALF_DAY_SESSION_TABLE.session(MarketSession::Normal),
            "only the one value that is a date was read as one"
        );
        assert_eq!(
            state.half_days_midnights(),
            ahash::AHashSet::from_iter([midnight_of(
                jiff::civil::date(2026, 11, 27),
                &jiff::tz::TimeZone::get(MARKET_ZONE).expect("the market zone is in the db"),
            )]),
            "only the one value that is a date was read as one, and it was read as its own day"
        );
    }

    /// A quote of an extended session contributes no print at all, a quote of the regular
    /// session carries the increment the venue's own counter advanced by, and a session
    /// number the venue has not defined is a failure rather than a guess.
    #[test]
    fn a_quote_outside_the_regular_session_is_no_print() {
        let state = state();

        for session in [1, 2, 3] {
            assert!(
                state
                    .quote_print(&quote(session, 1_781_097_600))
                    .expect("an extended session's quote is read")
                    .is_none(),
                "a quote of session {session} builds no candle"
            );
        }

        let print = state
            .quote_print(&quote(0, 1_781_097_600))
            .expect("a regular-session quote is read")
            .expect("and it is a print");
        assert_eq!(
            print.timestamp, 1_781_097_600_000,
            "the venue's seconds are read as milliseconds"
        );
        assert_eq!(print.volume_delta, Some(Quantity::from(1_u8)));
        assert!(
            print.cumulative.is_none(),
            "an hourly candle is not written from the day's counters"
        );

        assert!(
            state.quote_print(&quote(9, 1_781_097_600)).is_err(),
            "an unknown session is refused"
        );
    }

    /// A daily candle is written from the venue's cumulative counters, so a quote of
    /// the regular session carries them — including one that advanced no volume at all,
    /// because the counters are the candle rather than an increment of it.
    ///
    /// The frame is an end-of-day one, which §4.6 states is merged exactly as an ordinary
    /// quote is: the venue merges it before its own end-of-day judgement, and the tag's
    /// other consequence — that the frame is not forwarded as a ticker — is a surface this
    /// build does not serve.
    #[test]
    fn an_end_of_day_quote_carries_the_sessions_own_counters() {
        let mut state = PushState::new(TimeFrame::Day1);
        state.install_calendar(&[], Instant::now());
        let mut quote = quote(0, 1_781_097_600);
        quote.last_done = "115.00".to_owned();
        quote.open = "99.00".to_owned();
        quote.high = "120.00".to_owned();
        quote.low = "98.00".to_owned();
        quote.volume = 1_000;
        quote.current_volume = 0;
        quote.tag = 1;

        let print = state
            .quote_print(&quote)
            .expect("the quote is read")
            .expect("and it is a print even though it advanced no volume");
        let cumulative = print.cumulative.expect("a daily quote states the day");
        assert_eq!(cumulative.open.to_string(), "99");
        assert_eq!(cumulative.high.to_string(), "120");
        assert_eq!(cumulative.low.to_string(), "98");
        assert_eq!(cumulative.close.to_string(), "115");
        assert_eq!(cumulative.volume, Quantity::from(1_000_u64));
    }

    /// An interval above a day takes the increment its counters advanced by rather than
    /// the day's own counters: a quote that states a whole day is not a week's candle, so
    /// a weekly subscriber sees the day's counters advance rather than replace what it
    /// has.
    #[test]
    fn an_interval_above_a_day_carries_the_increment() {
        let state = PushState::new(TimeFrame::Week1);
        let mut quote = quote(0, 1_781_097_600);
        quote.last_done = "115.00".to_owned();
        quote.open = "99.00".to_owned();
        quote.high = "120.00".to_owned();
        quote.low = "98.00".to_owned();
        quote.volume = 1_000;
        quote.current_volume = 7;

        let print = state
            .quote_print(&quote)
            .expect("the quote is read")
            .expect("and it is a print");
        assert!(
            print.cumulative.is_none(),
            "a weekly candle is accumulated, not written from a day's counters"
        );
        assert_eq!(print.volume_delta, Some(Quantity::from(7_u8)));
    }

    /// A quote of the regular session that advanced no volume still states a price, so it
    /// is merged as a price with nothing added rather than dropped: a reader of an hourly
    /// candle sees the price move even on a second where the venue's counter did not.
    #[test]
    fn a_quote_that_advanced_no_volume_still_states_its_price() {
        let state = state();
        let mut quiet = quote(0, 1_781_097_600);
        quiet.current_volume = 0;
        quiet.current_turnover = String::new();

        let print = state
            .quote_print(&quiet)
            .expect("the quote is read")
            .expect("a price without a trade is still a price");
        assert_eq!(print.price, Some(Price::from(101_u8)));
        assert_eq!(
            print.volume_delta, None,
            "and it adds no volume to the candle"
        );

        let mut unchanged = quote(0, 1_781_097_600);
        unchanged.current_volume = 0;
        let repeated = state
            .quote_print(&unchanged)
            .expect("the quote is read")
            .expect("and it is read as the price it states");
        assert_eq!(
            repeated.price, print.price,
            "a quote that repeats the same price states the same price"
        );
        assert_eq!(
            repeated.volume_delta, None,
            "and still adds nothing, which is what a push without a trade is"
        );
    }

    /// One trade push is one print per entry of its own tape, each with its own session,
    /// price, volume and type.
    #[test]
    fn a_trade_push_is_one_print_per_trade() {
        let state = state();
        let trades = PushTrade {
            symbol: "AAPL.US".to_owned(),
            sequence: 7,
            trade: vec![
                Trade {
                    price: "101.50".to_owned(),
                    volume: 3,
                    timestamp: 1_781_097_600,
                    trade_type: String::new(),
                    direction: 0,
                    trade_session: 0,
                },
                Trade {
                    price: "99.25".to_owned(),
                    volume: 5,
                    timestamp: 1_781_097_660,
                    trade_type: "C".to_owned(),
                    direction: 1,
                    trade_session: 2,
                },
            ],
        };

        let prints = state.trade_prints(&trades).expect("the tape is read");
        assert_eq!(prints.len(), 2, "one print per trade of the frame");
        assert_eq!(prints[0].timestamp, 1_781_097_600_000);
        assert_eq!(prints[0].volume_delta, Some(Quantity::from(3_u8)));
        assert_eq!(
            prints[0].fields(),
            crate::adapter::longbridge::candles::aggregator::UpdatedFields::PriceAndVolume,
            "the ordinary print, which states no type, moves everything"
        );
        assert_eq!(
            prints[1].session,
            MarketSession::Post,
            "the second print states its own session"
        );
        assert_eq!(
            prints[1].fields(),
            crate::adapter::longbridge::candles::aggregator::UpdatedFields::VolumeOnly,
            "and its own type, which is read against that session"
        );
    }
}
