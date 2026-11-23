use longbridge_proto::quote::PushQuote;
use market_participants_model::decimal::{Price, Quantity};

use super::{
    CandleAggregator, CandleOutcome, CandlePrint, CumulativeDay, QuotePrint, TradePrint, TradeType,
    UpdatedFields,
};
use crate::{
    adapter::{
        NativeCandlestick,
        longbridge::candles::{
            push_state::PushState,
            session_table::{
                HALF_DAY_SESSION_TABLE, MARKET_ZONE, MarketSession, NORMAL_SESSION_TABLE,
                SessionTable,
            },
        },
    },
    market::timeframe::TimeFrame,
};

fn price(cents: u64) -> Price {
    Price::from(cents)
}

fn shares(count: u64) -> Quantity {
    Quantity::from(count)
}

fn at(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> i64 {
    jiff::civil::date(year, month, day)
        .at(hour, minute, 0, 0)
        .to_zoned(jiff::tz::TimeZone::get(MARKET_ZONE).expect("the market zone is in the database"))
        .expect("the fixture wall time is a real instant")
        .timestamp()
        .as_millisecond()
}

fn zone() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(MARKET_ZONE).expect("the market zone is in the database")
}

fn aggregator(timeframe: TimeFrame, bucket: i64) -> CandleAggregator {
    CandleAggregator::from_first_screen(
        timeframe,
        zone(),
        NativeCandlestick {
            timestamp: bucket,
            open: price(100),
            high: price(100),
            low: price(100),
            close: price(100),
            volume: shares(10),
        },
    )
}

fn trade(timestamp: i64, cents: u64, count: u64, trade_type: TradeType) -> CandlePrint {
    trade_of(MarketSession::Normal, timestamp, cents, count, trade_type)
}

fn trade_of(
    session: MarketSession,
    timestamp: i64,
    cents: u64,
    count: u64,
    trade_type: TradeType,
) -> CandlePrint {
    CandlePrint::trade(TradePrint {
        timestamp,
        session,
        price: price(cents),
        volume: shares(count),
        trade_type,
    })
}

fn quote(timestamp: i64, cents: u64, count: u64) -> CandlePrint {
    CandlePrint::quote(QuotePrint {
        timestamp,
        session: MarketSession::Normal,
        price: price(cents),
        volume_delta: Some(shares(count)),
        cumulative: None,
    })
}

fn quote_day(timestamp: i64, day: CumulativeDay) -> CandlePrint {
    CandlePrint::quote(QuotePrint {
        timestamp,
        session: MarketSession::Normal,
        price: day.close,
        volume_delta: Some(day.volume),
        cumulative: Some(day),
    })
}

fn table() -> SessionTable {
    NORMAL_SESSION_TABLE
}

#[track_caller]
fn assert_bucket(candle: &NativeCandlestick, year: i16, month: i8, day: i8, hour: i8, minute: i8) {
    assert_eq!(
        candle.timestamp,
        at(year, month, day, hour, minute),
        "the candle opens at {year}-{month:02}-{day:02} {hour:02}:{minute:02} in New York"
    );
}

#[test]
fn prints_of_one_bucket_update_one_candle() {
    let timestamp = at(2026, 6, 10, 10, 5);
    let mut aggregator = aggregator(TimeFrame::Minute5, at(2026, 6, 10, 10, 5));

    let first = aggregator.apply(&trade(timestamp, 105, 3, TradeType::Unnamed), &table());
    let CandleOutcome::Updated(first) = first else {
        panic!("a print inside the bucket updates it: {first:?}");
    };
    assert_eq!(first.close, price(105));
    assert_eq!(first.high, price(105), "the first print is the high so far");
    assert_eq!(first.volume, shares(13), "10 from the screen plus 3");

    let second = aggregator.apply(
        &trade(timestamp + 1_000, 95, 7, TradeType::Unnamed),
        &table(),
    );
    let CandleOutcome::Updated(second) = second else {
        panic!("a second print of the same bucket updates the same candle: {second:?}");
    };
    assert_eq!(
        second.open,
        price(100),
        "the open is the screen's, not rewritten"
    );
    assert_eq!(second.close, price(95));
    assert_eq!(
        second.high,
        price(105),
        "the high is the highest print so far"
    );
    assert_eq!(second.low, price(95));
    assert_eq!(second.volume, shares(20), "and the volume accumulates");
    assert_eq!(aggregator.candle().close, price(95));
}

#[test]
fn a_quote_accumulates_its_own_increment() {
    let timestamp = at(2026, 6, 10, 10, 6);
    let mut aggregator = aggregator(TimeFrame::Minute5, at(2026, 6, 10, 10, 5));

    let CandleOutcome::Updated(candle) = aggregator.apply(&quote(timestamp, 101, 4), &table())
    else {
        panic!("a quote of the regular session builds a candle");
    };

    assert_eq!(candle.close, price(101));
    assert_eq!(candle.volume, shares(14));
}

#[test]
fn a_daily_candle_is_the_sessions_own_counters() {
    let mut aggregator = aggregator(TimeFrame::Day1, at(2026, 6, 10, 0, 0));

    let CandleOutcome::Updated(candle) = aggregator.apply(
        &quote_day(
            at(2026, 6, 10, 10, 0),
            CumulativeDay {
                open: price(99),
                high: price(120),
                low: price(98),
                close: price(115),
                volume: shares(1_000),
            },
        ),
        &table(),
    ) else {
        panic!("a regular-session quote writes the day's candle");
    };

    assert_bucket(&candle, 2026, 6, 10, 0, 0);
    assert_eq!(candle.open, price(99));
    assert_eq!(candle.high, price(120));
    assert_eq!(candle.low, price(98));
    assert_eq!(candle.close, price(115));
    assert_eq!(
        candle.volume,
        shares(1_000),
        "the day's own volume, not a delta"
    );
}

#[test]
fn a_quote_of_a_longer_interval_accumulates_instead_of_replacing() {
    for (timeframe, bucket) in [
        (TimeFrame::Week1, at(2026, 6, 8, 0, 0)),
        (TimeFrame::Month1, at(2026, 6, 1, 0, 0)),
    ] {
        let state = PushState::new(timeframe);
        let mut aggregator = aggregator(timeframe, bucket);

        let CandleOutcome::Updated(first) = aggregator.apply(&state_quote(&state, 5), &table())
        else {
            panic!("a {timeframe:?} quote of the regular session updates its candle");
        };
        assert_eq!(
            first.open,
            price(100),
            "the candle opens where the screen left it, not at the day's own open"
        );
        assert_eq!(first.close, price(115), "the increment moves the close");
        assert_eq!(
            first.volume,
            shares(15),
            "and the volume is the screen's plus the increment the counters advanced by"
        );
        assert_eq!(
            first.timestamp, bucket,
            "which is still the period the screen left off at"
        );

        let CandleOutcome::Updated(second) = aggregator.apply(&state_quote(&state, 5), &table())
        else {
            panic!("a second {timeframe:?} quote updates the same candle");
        };
        assert_eq!(
            second.volume,
            shares(20),
            "so the same day's counters advance the candle twice rather than replacing it"
        );
        assert_eq!(second.close, price(115));
    }
}

#[test]
fn the_day_is_the_only_interval_written_from_the_counters() {
    let state = PushState::new(TimeFrame::Day1);
    let mut aggregator = aggregator(TimeFrame::Day1, at(2026, 6, 10, 0, 0));

    let CandleOutcome::Updated(candle) = aggregator.apply(&state_quote(&state, 5), &table()) else {
        panic!("a daily quote writes the day's candle");
    };

    assert_eq!(candle.open, price(99), "the day's own open");
    assert_eq!(candle.high, price(120));
    assert_eq!(candle.low, price(98));
    assert_eq!(candle.close, price(115));
    assert_eq!(
        candle.volume,
        shares(1_000),
        "and the day's own volume rather than an increment of it"
    );
}

fn state_quote(state: &PushState, current_volume: i64) -> CandlePrint {
    let quote = PushQuote {
        symbol: "AAPL.US".to_owned(),
        last_done: "115.00".to_owned(),
        open: "99.00".to_owned(),
        high: "120.00".to_owned(),
        low: "98.00".to_owned(),
        timestamp: at(2026, 6, 10, 10, 0) / 1_000,
        volume: 1_000,
        current_volume,
        current_turnover: "575.00".to_owned(),
        trade_status: 0,
        trade_session: 0,
        sequence: 1,
        turnover: String::new(),
        tag: 0,
    };
    state
        .quote_print(&quote)
        .expect("the fixture quote is read")
        .expect("a regular-session quote is a print")
}
#[test]
fn the_i_row_reads_the_prints_own_session() {
    let regular = trade(at(2026, 6, 10, 10, 5), 500, 4, TradeType::of("I"));
    assert_eq!(regular.session, MarketSession::Normal);
    assert_eq!(
        regular.fields(),
        UpdatedFields::VolumeOnly,
        "`I` on a regular-session share moves the volume only"
    );

    for (session, timestamp) in [
        (MarketSession::Pre, at(2026, 6, 10, 9, 20)),
        (MarketSession::Post, at(2026, 6, 10, 18, 0)),
    ] {
        let extended = trade_of(session, timestamp, 500, 4, TradeType::of("I"));
        assert_eq!(
            extended.session, session,
            "the print states the session the venue filed it under"
        );
        assert_eq!(
            extended.fields(),
            UpdatedFields::PriceAndVolume,
            "an `I` print of the {session:?} session is the rule's other arm and moves everything"
        );
    }

    let overnight = trade_of(
        MarketSession::Overnight,
        at(2026, 6, 11, 1, 15),
        500,
        4,
        TradeType::of("I"),
    );
    assert_eq!(
        overnight.session,
        MarketSession::Overnight,
        "the fixture states the overnight session"
    );
    assert_eq!(
        overnight.fields(),
        UpdatedFields::VolumeOnly,
        "`I` on an overnight share moves the volume only, not the price"
    );

    let mut aggregator = aggregator(TimeFrame::Minute1, at(2026, 6, 10, 20, 0));
    let CandleOutcome::Rolled { opened, .. } = aggregator.apply(&overnight, &table()) else {
        panic!("01:15 opens the small hours' first minute");
    };
    assert_bucket(&opened, 2026, 6, 11, 1, 15);
    assert_eq!(
        opened.high,
        price(100),
        "an overnight `I` print does not move the high"
    );
    assert_eq!(
        opened.close,
        price(100),
        "and it does not move the close, which stays where the bucket opened"
    );
    assert_eq!(
        opened.volume,
        shares(4),
        "while the volume it states is accumulated"
    );
}

#[test]
fn a_volume_only_print_of_a_newer_bucket_rolls_and_accumulates() {
    let mut aggregator = aggregator(TimeFrame::Minute1, at(2026, 6, 10, 10, 5));

    let CandleOutcome::Rolled { confirmed, opened } = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 6), 500, 4, TradeType::of("C")),
        &table(),
    ) else {
        panic!("a volume-only print of the next minute opens that minute");
    };

    assert_bucket(&confirmed, 2026, 6, 10, 10, 5);
    assert_bucket(&opened, 2026, 6, 10, 10, 6);
    assert_eq!(opened.open, confirmed.close, "the new candle opens at it");
    assert_eq!(
        opened.volume,
        shares(4),
        "and holds the volume of the print that opened it"
    );
    assert_eq!(
        opened.close, confirmed.close,
        "a print that states no price leaves the close where it opened"
    );

    let CandleOutcome::Updated(same_minute) = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 6), 700, 5, TradeType::of("H")),
        &table(),
    ) else {
        panic!("a second volume-only print of that minute updates it");
    };
    assert_eq!(same_minute.volume, shares(9), "and accumulates into it");
    assert_eq!(same_minute.close, confirmed.close);
    assert_eq!(
        same_minute.open, confirmed.close,
        "a volume-only print of that minute leaves the open where the candle opened"
    );
    assert_eq!(
        same_minute.high, confirmed.close,
        "and nothing that states a price has moved the high yet"
    );
}

#[test]
fn a_price_print_rewrites_the_open_a_volume_only_print_left() {
    let mut aggregator = aggregator(TimeFrame::Minute1, at(2026, 6, 10, 10, 5));

    let CandleOutcome::Rolled { confirmed, opened } = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 6), 500, 4, TradeType::of("C")),
        &table(),
    ) else {
        panic!("a volume-only print of the next minute opens that minute");
    };
    assert_eq!(opened.open, confirmed.close, "the new candle opens at it");
    assert_eq!(opened.volume, shares(4));
    assert_eq!(opened.high, price(100), "and no print has moved it yet");

    let CandleOutcome::Updated(bucket) = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 6), 90, 2, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("an ordinary print of that minute updates it");
    };
    assert_eq!(
        bucket.open,
        price(90),
        "the first price the bucket sees is its open, not the previous candle's close"
    );
    assert_eq!(bucket.close, price(90));
    assert_eq!(
        bucket.high,
        price(100),
        "and the high is the inherited close the print did not move past, which stays inside \
         the range as the venue's own merger leaves it"
    );
    assert_eq!(bucket.low, price(90));
    assert_eq!(bucket.volume, shares(6), "with both prints' volume");

    let CandleOutcome::Updated(after) = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 6), 95, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("a second price print updates the same bucket");
    };
    assert_eq!(after.open, price(90), "the open is the first price, once");
    assert_eq!(after.close, price(95));
    assert_eq!(after.high, price(100), "and the high is the best of them");
    assert_eq!(after.low, price(90));
    assert_eq!(after.volume, shares(7));
}

#[test]
fn an_hourly_bucket_starts_at_the_sessions_open() {
    let mut aggregator = aggregator(TimeFrame::Hour1, at(2026, 6, 10, 9, 30));

    let CandleOutcome::Updated(inside) = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 0), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("10:00 is inside the 09:30 candle");
    };
    assert_bucket(&inside, 2026, 6, 10, 9, 30);

    let CandleOutcome::Rolled { confirmed, opened } = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 30), 102, 2, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("10:30 opens the next hourly candle");
    };
    assert_bucket(&confirmed, 2026, 6, 10, 9, 30);
    assert_bucket(&opened, 2026, 6, 10, 10, 30);
    assert_eq!(
        opened.open,
        price(102),
        "the print that opened the candle is its open, high, low and close (§4.6 step 3)"
    );
    assert_eq!(opened.high, price(102));
    assert_eq!(opened.low, price(102));
    assert_eq!(
        opened.close,
        price(102),
        "and the previous candle's close is gone, because a real trade replaced it"
    );
    assert_eq!(opened.volume, shares(2));
}

#[test]
fn a_four_hour_bucket_restarts_at_each_session() {
    let mut aggregator = aggregator(TimeFrame::Hour4, at(2026, 6, 10, 9, 30));

    let CandleOutcome::Updated(morning) = aggregator.apply(
        &trade(at(2026, 6, 10, 13, 0), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("13:00 is still inside the 09:30 candle");
    };
    assert_bucket(&morning, 2026, 6, 10, 9, 30);

    let CandleOutcome::Rolled {
        opened: afternoon, ..
    } = aggregator.apply(
        &trade(at(2026, 6, 10, 13, 30), 102, 1, TradeType::Unnamed),
        &table(),
    )
    else {
        panic!("13:30 opens the afternoon candle");
    };
    assert_bucket(&afternoon, 2026, 6, 10, 13, 30);

    let CandleOutcome::Rolled {
        opened: evening, ..
    } = aggregator.apply(
        &trade(at(2026, 6, 10, 16, 0), 103, 1, TradeType::Unnamed),
        &table(),
    )
    else {
        panic!("16:00 opens the post session's first candle");
    };
    assert_bucket(&evening, 2026, 6, 10, 16, 0);
}

#[test]
fn an_overnight_bucket_is_counted_from_the_window_it_fell_in() {
    let mut overnight = aggregator(TimeFrame::Hour1, at(2026, 6, 10, 20, 0));

    let CandleOutcome::Updated(evening) = overnight.apply(
        &trade(at(2026, 6, 10, 20, 30), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("20:30 is the overnight session's first hourly candle");
    };
    assert_bucket(&evening, 2026, 6, 10, 20, 0);

    let CandleOutcome::Rolled {
        opened: last_hour, ..
    } = overnight.apply(
        &trade(at(2026, 6, 10, 23, 45), 102, 1, TradeType::Unnamed),
        &table(),
    )
    else {
        panic!("23:45 is the venue's last evening bucket, and the last of the day");
    };
    assert_bucket(&last_hour, 2026, 6, 10, 23, 0);

    let CandleOutcome::Rolled {
        opened: after_midnight,
        ..
    } = overnight.apply(
        &trade(at(2026, 6, 11, 1, 15), 103, 2, TradeType::Unnamed),
        &table(),
    )
    else {
        panic!("01:15 opens the small hours' own hourly candle");
    };
    assert_bucket(&after_midnight, 2026, 6, 11, 1, 0);
    assert_eq!(
        after_midnight.open,
        price(103),
        "which opens at the print that opened it, rather than at the day's last close"
    );
    assert_eq!(after_midnight.close, price(103));
    assert_eq!(after_midnight.volume, shares(2));

    let mut forty_five = aggregator(TimeFrame::Minute45, at(2026, 6, 10, 23, 45));
    let CandleOutcome::Rolled {
        opened: after_midnight,
        ..
    } = forty_five.apply(
        &trade(at(2026, 6, 11, 0, 30), 104, 1, TradeType::Unnamed),
        &table(),
    )
    else {
        panic!("00:30 opens the bucket the venue publishes after 23:45");
    };
    assert_bucket(&after_midnight, 2026, 6, 11, 0, 30);

    let before = overnight.candle().clone();
    let CandleOutcome::Rolled { confirmed, opened } = overnight.apply(
        &trade(at(2026, 6, 11, 4, 0), 105, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("04:00 opens the pre session's first hourly candle");
    };
    assert_eq!(confirmed, before, "the small hours' candle is confirmed");
    assert_bucket(&opened, 2026, 6, 11, 4, 0);
    assert_eq!(
        opened.open,
        price(105),
        "and the pre session's grid opens at the print that opened it"
    );
    assert_eq!(
        opened.volume,
        shares(1),
        "which carries that print's own volume"
    );
}

#[test]
fn an_older_bucket_is_stale() {
    let mut aggregator = aggregator(TimeFrame::Minute5, at(2026, 6, 10, 10, 5));

    let CandleOutcome::Rolled { .. } = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 11), 102, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("10:11 opens the next five-minute candle");
    };
    let before = aggregator.candle().clone();

    assert_eq!(
        aggregator.apply(
            &trade(at(2026, 6, 10, 10, 6), 90, 5, TradeType::Unnamed),
            &table()
        ),
        CandleOutcome::Stale,
        "a print of an older bucket is not merged into the open candle"
    );
    assert_eq!(
        aggregator.candle(),
        &before,
        "and the open candle is exactly what it was"
    );
}

#[test]
fn a_daily_bucket_is_the_markets_midnight() {
    let mut aggregator = aggregator(TimeFrame::Day1, at(2026, 6, 10, 0, 0));

    let CandleOutcome::Updated(same_day) = aggregator.apply(
        &trade(at(2026, 6, 10, 15, 59), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("the same local day is the same candle");
    };
    assert_bucket(&same_day, 2026, 6, 10, 0, 0);

    let CandleOutcome::Rolled { confirmed, opened } = aggregator.apply(
        &trade(at(2026, 6, 11, 9, 31), 102, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("the next local day opens the next daily candle");
    };
    assert_bucket(&confirmed, 2026, 6, 10, 0, 0);
    assert_bucket(&opened, 2026, 6, 11, 0, 0);
}

#[test]
fn a_weekly_bucket_is_the_monday_of_the_markets_week() {
    let mut aggregator = aggregator(TimeFrame::Week1, at(2026, 6, 8, 0, 0));

    let CandleOutcome::Updated(wednesday) = aggregator.apply(
        &trade(at(2026, 6, 10, 11, 0), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("Wednesday belongs to the week that opened on Monday");
    };
    assert_bucket(&wednesday, 2026, 6, 8, 0, 0);

    let CandleOutcome::Rolled { opened, .. } = aggregator.apply(
        &trade(at(2026, 6, 15, 11, 0), 102, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("the next Monday opens the next weekly candle");
    };
    assert_bucket(&opened, 2026, 6, 15, 0, 0);
}

#[test]
fn an_extended_session_never_builds_a_daily_scale_candle() {
    for timeframe in [
        TimeFrame::Day1,
        TimeFrame::Week1,
        TimeFrame::Month1,
        TimeFrame::Quarter1,
        TimeFrame::Year1,
    ] {
        let mut aggregator = aggregator(timeframe, at(2026, 6, 10, 0, 0));
        for session in [
            MarketSession::Pre,
            MarketSession::Post,
            MarketSession::Overnight,
        ] {
            let print = trade_of(session, at(2026, 6, 10, 18, 0), 101, 1, TradeType::Unnamed);

            assert_eq!(
                aggregator.apply(&print, &table()),
                CandleOutcome::Stale,
                "{session:?} never builds a {timeframe:?} candle"
            );
            assert_eq!(
                aggregator.candle().volume,
                shares(10),
                "and the candle being built is untouched"
            );
        }
    }
}

#[test]
fn an_extended_session_reaches_only_a_below_daily_candle() {
    let mut premarket = aggregator(TimeFrame::Minute1, at(2026, 6, 10, 9, 0));
    let print = trade_of(
        MarketSession::Pre,
        at(2026, 6, 10, 9, 20),
        101,
        3,
        TradeType::Unnamed,
    );

    let CandleOutcome::Rolled { opened, .. } = premarket.apply(&print, &table()) else {
        panic!("a pre-market print builds a one-minute candle");
    };
    assert_bucket(&opened, 2026, 6, 10, 9, 20);
    assert_eq!(
        opened.open,
        price(101),
        "which opens at the print that opened it, the venue's own rule"
    );
    assert_eq!(
        opened.close,
        price(101),
        "and closes at the print's own price"
    );
}

#[test]
fn an_extended_session_print_reaches_only_its_own_bucket() {
    let mut premarket_quote = aggregator(TimeFrame::Day1, at(2026, 6, 10, 0, 0));
    let mut extended = quote(at(2026, 6, 10, 9, 20), 101, 3);
    extended.session = MarketSession::Pre;
    assert_eq!(
        premarket_quote.apply(&extended, &table()),
        CandleOutcome::Stale,
        "a pre-market quote builds no daily candle"
    );

    let mut minute = aggregator(TimeFrame::Minute1, at(2026, 6, 10, 9, 20));
    let CandleOutcome::Updated(updated) = minute.apply(&extended, &table()) else {
        panic!("an extended session's own bucket is a candle below the daily scale");
    };
    assert_bucket(&updated, 2026, 6, 10, 9, 20);
    assert_eq!(updated.close, price(101));
    assert_eq!(updated.volume, shares(13));
}

#[test]
fn the_trade_type_table_is_the_venues_own() {
    for value in ["", "A", "B", "D", "E", "F", "K", "S", "X", "1"] {
        assert_eq!(
            UpdatedFields::of(TradeType::of(value), MarketSession::Normal),
            UpdatedFields::PriceAndVolume,
            "`{value}` updates the price and the volume"
        );
    }
    for value in ["C", "G", "H", "V", "W"] {
        assert_eq!(
            UpdatedFields::of(TradeType::of(value), MarketSession::Normal),
            UpdatedFields::VolumeOnly,
            "`{value}` updates the volume only"
        );
    }
    assert_eq!(
        UpdatedFields::of(TradeType::of("I"), MarketSession::Normal),
        UpdatedFields::VolumeOnly,
        "`I` is volume-only on a share, which is every board this build publishes"
    );
    for value in ["M", "P", "Z", "Q", " ", "AA"] {
        assert_eq!(
            UpdatedFields::of(TradeType::of(value), MarketSession::Normal),
            UpdatedFields::Nothing,
            "`{value}` updates nothing"
        );
    }
    assert_eq!(
        TradeType::of(""),
        TradeType::Unnamed,
        "the venue's ordinary print states no type at all"
    );
    assert_eq!(
        TradeType::of("Z"),
        TradeType::Unknown,
        "and a value outside the venue's own table is a type this build does not know"
    );
    assert_ne!(
        TradeType::of("M"),
        TradeType::Unknown,
        "`M` is a type the venue names, and its row is what makes it update nothing"
    );
}

#[test]
fn a_volume_only_print_leaves_the_price_and_a_named_nothing_leaves_everything() {
    let mut aggregator = aggregator(TimeFrame::Minute1, at(2026, 6, 10, 10, 5));

    let CandleOutcome::Updated(volume_only) = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 5), 500, 4, TradeType::of("C")),
        &table(),
    ) else {
        panic!("a volume-only print still updates the candle");
    };
    assert_eq!(volume_only.open, price(100));
    assert_eq!(volume_only.high, price(100), "`C` does not move the high");
    assert_eq!(volume_only.low, price(100));
    assert_eq!(volume_only.close, price(100), "`C` does not move the close");
    assert_eq!(volume_only.volume, shares(14));

    let CandleOutcome::Updated(nothing) = aggregator.apply(
        &trade(at(2026, 6, 10, 10, 5), 700, 9, TradeType::of("M")),
        &table(),
    ) else {
        panic!("a type the table does not name still answers with the candle");
    };
    assert_eq!(
        nothing, volume_only,
        "`M` moves neither the price nor the volume"
    );
}

#[test]
fn a_volume_only_print_opens_no_bucket_out_of_nothing() {
    let gapped = SessionTable::new(
        (4, 0, 9, 30),
        (9, 30, 16, 0),
        (16, 0, 17, 0),
        ((20, 0), (4, 0)),
    );
    let mut aggregator = aggregator(TimeFrame::Minute1, at(2026, 6, 10, 10, 5));
    let before = aggregator.candle().clone();

    assert_eq!(
        aggregator.apply(
            &trade(at(2026, 6, 10, 18, 0), 500, 4, TradeType::of("H")),
            &gapped
        ),
        CandleOutcome::Stale,
        "18:00 is outside every window of that table, so it has no bucket at all"
    );
    assert_eq!(aggregator.candle(), &before);
}

#[test]
fn a_frame_outside_every_session_is_dropped() {
    let mut aggregator = aggregator(TimeFrame::Minute1, at(2026, 6, 10, 10, 5));
    let before = aggregator.candle().clone();
    let gapped = SessionTable::new(
        (4, 0, 9, 30),
        (9, 30, 16, 0),
        (16, 0, 17, 0),
        ((20, 0), (4, 0)),
    );

    assert_eq!(
        aggregator.apply(
            &trade(at(2026, 6, 10, 17, 30), 101, 1, TradeType::Unnamed),
            &gapped
        ),
        CandleOutcome::Stale,
        "17:30 is between that table's post session and its overnight one"
    );
    assert_eq!(aggregator.candle(), &before);
}

#[test]
fn a_half_day_rests_the_afternoon_grid_on_thirteen() {
    let mut ordinary = aggregator(TimeFrame::Hour4, at(2026, 11, 27, 9, 30));
    let mut half = aggregator(TimeFrame::Hour4, at(2026, 11, 27, 9, 30));

    let CandleOutcome::Rolled {
        opened: ordinary_candle,
        ..
    } = ordinary.apply(
        &trade(at(2026, 11, 27, 16, 30), 101, 1, TradeType::Unnamed),
        &NORMAL_SESSION_TABLE,
    )
    else {
        panic!("the ordinary grid opens its post candle at 16:00");
    };
    let CandleOutcome::Rolled {
        opened: half_candle,
        ..
    } = half.apply(
        &trade(at(2026, 11, 27, 16, 30), 101, 1, TradeType::Unnamed),
        &HALF_DAY_SESSION_TABLE,
    )
    else {
        panic!("the half day's grid has a candle covering 16:30");
    };

    assert_bucket(&ordinary_candle, 2026, 11, 27, 16, 0);
    assert_bucket(&half_candle, 2026, 11, 27, 13, 0);
    assert_eq!(
        ordinary_candle.volume, half_candle.volume,
        "and both candles hold the volume of the same print"
    );
}

#[test]
fn the_buckets_follow_the_markets_daylight_saving() {
    let mut spring = aggregator(TimeFrame::Day1, at(2026, 3, 8, 0, 0));
    let CandleOutcome::Updated(spring_day) = spring.apply(
        &trade(at(2026, 3, 8, 12, 0), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("a regular-session trade of the transition day builds its candle");
    };
    assert_bucket(&spring_day, 2026, 3, 8, 0, 0);
    assert_eq!(
        spring_day.timestamp, 1_772_946_000_000,
        "2026-03-08T00:00 in New York is 05:00 UTC, not 04:00"
    );

    let mut spring_hour = aggregator(TimeFrame::Hour1, at(2026, 3, 9, 9, 30));
    let CandleOutcome::Updated(monday) = spring_hour.apply(
        &trade(at(2026, 3, 9, 9, 45), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("the Monday after the transition is a trading day");
    };
    assert_bucket(&monday, 2026, 3, 9, 9, 30);
    assert_eq!(
        monday.timestamp, 1_773_063_000_000,
        "2026-03-09T09:30 in New York is 13:30 UTC, because the offset is now -04:00"
    );
    assert_eq!(
        monday.timestamp - spring_day.timestamp,
        32 * 60 * 60 * 1_000 + 30 * 60 * 1_000,
        "and the two candles are a local midnight and a local 09:30 apart: 32.5 local \
         hours, which is 33.5 UTC hours because the clock moved between them"
    );

    let mut autumn = aggregator(TimeFrame::Day1, at(2026, 11, 1, 0, 0));
    let CandleOutcome::Updated(autumn_day) = autumn.apply(
        &trade(at(2026, 11, 1, 12, 0), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("the November transition day is a trading day");
    };
    assert_eq!(
        autumn_day.timestamp, 1_793_505_600_000,
        "2026-11-01T00:00 in New York is 04:00 UTC, not 05:00"
    );
    assert_bucket(&autumn_day, 2026, 11, 1, 0, 0);

    let mut autumn_hour = aggregator(TimeFrame::Hour1, at(2026, 11, 2, 9, 30));
    let CandleOutcome::Updated(after) = autumn_hour.apply(
        &trade(at(2026, 11, 2, 10, 0), 101, 1, TradeType::Unnamed),
        &table(),
    ) else {
        panic!("the Monday after the November transition is a trading day");
    };
    assert_bucket(&after, 2026, 11, 2, 9, 30);
    assert_eq!(
        after.timestamp, 1_793_629_800_000,
        "2026-11-02T09:30 in New York is 14:30 UTC, because the offset is now -05:00"
    );
}
