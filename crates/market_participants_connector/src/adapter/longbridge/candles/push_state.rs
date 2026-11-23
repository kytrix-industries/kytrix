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

pub(super) const HALF_DAYS_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

const DECODE_PUSH: &str = "decode the Longbridge push";

pub(super) struct PushState {
    timeframe: TimeFrame,
    zone: jiff::tz::TimeZone,
    half_days: Option<HalfDays>,
}

struct HalfDays {
    midnights: ahash::AHashSet<UnixMillis>,
    due: std::time::Instant,
}

impl PushState {
    pub(super) fn new(timeframe: TimeFrame) -> Self {
        Self {
            timeframe,
            zone: jiff::tz::TimeZone::get(MARKET_ZONE).unwrap_or(jiff::tz::TimeZone::UTC),
            half_days: None,
        }
    }

    pub(super) fn half_days_due(&self, now: std::time::Instant) -> bool {
        self.half_days
            .as_ref()
            .is_none_or(|half_days| now >= half_days.due)
    }

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

    #[cfg(test)]
    pub(super) fn half_days_midnights(&self) -> ahash::AHashSet<UnixMillis> {
        self.half_days
            .as_ref()
            .map(|half_days| half_days.midnights.clone())
            .unwrap_or_default()
    }

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

    pub(super) fn market_day(&self, millis: UnixMillis) -> Option<jiff::civil::Date> {
        jiff::Timestamp::from_millisecond(millis)
            .ok()
            .map(|timestamp| jiff::Zoned::new(timestamp, self.zone.clone()).date())
    }

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

fn seconds_to_millis(seconds: i64) -> Result<UnixMillis, MarketParticipantError> {
    seconds
        .checked_mul(1_000)
        .ok_or_else(|| MarketParticipantError::ProtocolError {
            operation: smol_str::SmolStr::new_static(DECODE_PUSH),
            reason: smol_str::format_smolstr!("a timestamp of {seconds} seconds overflows"),
            raw_data: None,
        })
}

fn decimal(value: &str) -> Result<Price, MarketParticipantError> {
    value
        .parse()
        .map_err(|_| MarketParticipantError::ProtocolError {
            operation: smol_str::SmolStr::new_static(DECODE_PUSH),
            reason: smol_str::format_smolstr!("`{value}` is not a decimal"),
            raw_data: None,
        })
}

fn volume_of(volume: i64) -> Result<Quantity, MarketParticipantError> {
    u64::try_from(volume)
        .map(Quantity::from)
        .map_err(|_| MarketParticipantError::ProtocolError {
            operation: smol_str::SmolStr::new_static(DECODE_PUSH),
            reason: smol_str::format_smolstr!("a volume of {volume} is not a quantity"),
            raw_data: None,
        })
}

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

fn named_type(trade_type: &str) -> TradeType {
    TradeType::of(trade_type)
}

fn parse_day(text: &str) -> Option<jiff::civil::Date> {
    jiff::civil::Date::new(
        i16::try_from(digits(text, 0..4)?).ok()?,
        i8::try_from(digits(text, 4..6)?).ok()?,
        i8::try_from(digits(text, 6..8)?).ok()?,
    )
    .ok()
}

fn digits(text: &str, range: std::ops::Range<usize>) -> Option<i32> {
    let bytes = text.as_bytes().get(range)?;
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn midnight_of(date: jiff::civil::Date, zone: &jiff::tz::TimeZone) -> UnixMillis {
    date.at(0, 0, 0, 0)
        .to_zoned(zone.clone())
        .map(|midnight| midnight.timestamp().as_millisecond())
        .unwrap_or(UnixMillis::MIN)
}

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

    fn at(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> UnixMillis {
        jiff::civil::date(year, month, day)
            .at(hour, minute, 0, 0)
            .to_zoned(jiff::tz::TimeZone::get(MARKET_ZONE).expect("the market zone is in the db"))
            .expect("the fixture wall time is a real instant")
            .timestamp()
            .as_millisecond()
    }

    fn state() -> PushState {
        PushState::new(TimeFrame::Hour1)
    }

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
