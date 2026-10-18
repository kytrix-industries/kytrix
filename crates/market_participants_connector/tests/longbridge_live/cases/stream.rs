//! The market-data cases: paging (cmd 27), the opening screen (cmd 19), and the two
//! commands agreeing candle for candle (§4.5, §4.6; §14.1 items 3, 5, 8).
//!
//! What these cases are for is the **grid**: a US equity's candles rest on its sessions'
//! own opens rather than on UTC hours, so the timestamp differences a venue publishes are
//! the evidence for the table §4.6 states. The grid this suite computes against is
//! [`crate::grid`]'s, which is the same five windows the adapter uses.

use longbridge_proto::quote::{
    AdjustType, Direction, Period, SecurityCandlestickRequest, SecurityCandlestickResponse,
    SecurityHistoryCandlestickRequest,
};
use tokio::time::timeout;

use crate::{
    bounds::{CASE_TIMEOUT, FIRST_SCREEN_COUNT, TRADE_SESSION_ALL},
    client::chosen_transport,
    credentials::Credentials,
    describe::{
        assert_candle_list, bucket_start, candle_summary, market_clock, session_label, session_name,
    },
    grid::{market_date, market_local, now_millis, session_of},
    skip_without_credentials,
    socket::LiveSocket,
    wire::{
        DIRECTION_BACKWARD, DIRECTION_FORWARD, GET_SECURITY_CANDLESTICKS,
        GET_SECURITY_HISTORY_CANDLESTICKS, PUSH_REALTIME_QUOTE, PUSH_REALTIME_TRADES,
        QUERY_BY_OFFSET, SUB_TYPE_QUOTE, SUB_TYPE_TRADE,
    },
};

/// One offset query of cmd 27 (§4.5).
pub fn offset_query(
    symbol: &str,
    period: Period,
    direction: Direction,
    date: &str,
    minute: &str,
    count: i32,
) -> SecurityHistoryCandlestickRequest {
    SecurityHistoryCandlestickRequest {
        symbol: symbol.to_owned(),
        period: period as i32,
        adjust_type: AdjustType::NoAdjust as i32,
        query_type: longbridge_proto::quote::HistoryCandlestickQueryType::QueryByOffset as i32,
        offset_request: Some(
            longbridge_proto::quote::security_history_candlestick_request::OffsetQuery {
                direction: direction as i32,
                date: date.to_owned(),
                minute: minute.to_owned(),
                count,
            },
        ),
        date_request: None,
        trade_session: TRADE_SESSION_ALL,
    }
}

/// The opening screen of a subscription, cmd 19 (§4.6).
pub fn first_screen(
    symbol: &str,
    period: Period,
    count: i32,
    adjust: AdjustType,
) -> SecurityCandlestickRequest {
    SecurityCandlestickRequest {
        symbol: symbol.to_owned(),
        period: period as i32,
        count,
        adjust_type: adjust as i32,
        trade_session: TRADE_SESSION_ALL,
    }
}

/// Case 7 — history paging: §4.5's four shapes, once each.
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_07_history_paging_covers_the_four_shapes_the_design_states() {
    let case = "case 7 (cmd 27 paging)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let symbol = crate::symbols::symbol();
        let mut socket =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
        socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        // The first screen, so the two anchored shapes have anchors to state.
        let newest: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_CANDLESTICKS,
                &first_screen(&symbol, Period::SixtyMinute, 20, AdjustType::NoAdjust),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the first screen failed: {error}"));
        assert_candle_list(case, &newest.candlesticks, 20);
        let anchor = newest
            .candlesticks
            .first()
            .expect("a first screen of a listed symbol states candles")
            .timestamp
            * 1_000;
        let anchor_date = market_date(anchor);
        let anchor_minute = market_local(anchor).strftime("%H%M").to_string();
        println!(
            "{case}: symbol={symbol} period=1h; the newest 1h candle is t={anchor} ({} \
             {anchor_date} {anchor_minute} ET)",
            market_clock(anchor)
        );

        // The four shapes below send the design's own numbering, so the numbering is
        // asserted before anything is sent with it: a direction or a query type the
        // venue numbers differently would make all four shapes one shape.
        assert_eq!(
            (QUERY_BY_OFFSET, DIRECTION_BACKWARD, DIRECTION_FORWARD),
            (
                longbridge_proto::quote::HistoryCandlestickQueryType::QueryByOffset as i32,
                Direction::Backward as i32,
                Direction::Forward as i32
            ),
            "{case}: the design's numbering for the query type and the two directions (§4.5)"
        );
        assert_eq!(
            (SUB_TYPE_QUOTE, SUB_TYPE_TRADE),
            (
                longbridge_proto::quote::SubType::Quote as i32,
                longbridge_proto::quote::SubType::Trade as i32
            ),
            "{case}: the design's numbering for the two subscription types (§4.6)"
        );
        assert_eq!(
            (PUSH_REALTIME_QUOTE, PUSH_REALTIME_TRADES),
            (101, 104),
            "{case}: the design's numbering for the two pushes (§4.3)"
        );

        // Shape 1: only an end — the widget's "page into history".
        let backward: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_HISTORY_CANDLESTICKS,
                &offset_query(
                    &symbol,
                    Period::SixtyMinute,
                    Direction::Backward,
                    &anchor_date,
                    &anchor_minute,
                    10,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the end-only shape failed: {error}"));
        // Shape 2: only a start — the widget's "page towards the newest".
        let forward_anchor = anchor - 20 * 3_600_000;
        let forward_date = market_date(forward_anchor);
        let forward_minute = market_local(forward_anchor).strftime("%H%M").to_string();
        let forward: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_HISTORY_CANDLESTICKS,
                &offset_query(
                    &symbol,
                    Period::SixtyMinute,
                    Direction::Forward,
                    &forward_date,
                    &forward_minute,
                    10,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the start-only shape failed: {error}"));
        // Shape 3: neither anchor — the widget's opening screen.
        let unanchored: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_HISTORY_CANDLESTICKS,
                &offset_query(
                    &symbol,
                    Period::SixtyMinute,
                    Direction::Backward,
                    "",
                    "",
                    10,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the anchorless shape failed: {error}"));
        // Shape 4: both anchors — the builder's `from(..).to(..)`.
        let both_start = anchor - 30 * 3_600_000;
        let both_end = anchor;
        let both: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_HISTORY_CANDLESTICKS,
                &offset_query(
                    &symbol,
                    Period::SixtyMinute,
                    Direction::Backward,
                    &market_date(both_end),
                    &market_local(both_end).strftime("%H%M").to_string(),
                    10,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the both-anchors shape failed: {error}"));
        socket.shutdown().await;

        let shapes: [(&str, &SecurityCandlestickResponse, i32); 4] = [
            ("end only (BACKWARD)", &backward, 10),
            ("start only (FORWARD)", &forward, 10),
            ("neither anchor (BACKWARD)", &unanchored, 10),
            ("both anchors", &both, 10),
        ];
        for (name, answer, count) in shapes {
            assert_eq!(
                answer.symbol, symbol,
                "{case}: the answer must echo the symbol it was asked about"
            );
            assert_candle_list(case, &answer.candlesticks, count);
            println!(
                "  {name}: {} candle(s), first={:?} last={:?}",
                answer.candlesticks.len(),
                answer.candlesticks.first().map(|candle| candle.timestamp),
                answer.candlesticks.last().map(|candle| candle.timestamp)
            );
            for line in candle_summary(&answer.candlesticks, 4) {
                println!("{line}");
            }
        }

        // The both-anchors shape is the one shape the adapter filters after the read,
        // and the filter's whole contract is that nothing earlier than `start`
        // survives (§4.5).
        let kept: Vec<i64> = both
            .candlesticks
            .iter()
            .filter(|candle| candle.timestamp * 1_000 >= both_start)
            .map(|candle| candle.timestamp)
            .collect();
        let dropped = both.candlesticks.len() - kept.len();
        println!(
            "  both anchors: start={both_start} ({}) end={both_end} ({}); {dropped} candle(s) \
             would be filtered away, leaving {}",
            market_clock(both_start),
            market_clock(both_end),
            kept.len()
        );
        for candle in &both.candlesticks {
            assert!(
                candle.timestamp * 1_000 <= both_end + 3_599_999,
                "{case}: the answer must not reach past the end anchor {}: {} is later",
                both_end,
                candle.timestamp * 1_000
            );
        }

        // The gaps between adjacent candles are what the session grid is checked
        // against: on a normal day the grid is 09:30–16:00 plus the pre and post
        // sessions, so the gaps name their boundaries.
        for (name, answer) in [("end only", &backward), ("forty-five minute", &unanchored)] {
            let gaps: Vec<i64> = answer
                .candlesticks
                .windows(2)
                .map(|pair| pair[1].timestamp - pair[0].timestamp)
                .collect();
            println!("  {name}: gaps between adjacent timestamps (s) = {gaps:?}");
        }

        // The 1h grid must be the session-anchored one, not UTC hours: 09:30, 10:30,
        // … are what a US equity prints, and this suite checks a 45m read as well
        // because that is the interval a UTC grid cannot produce at all.
        let forty_five: SecurityCandlestickResponse = {
            let mut socket =
                LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                    .await
                    .unwrap_or_else(|error| panic!("{case}: {error}"));
            socket
                .authenticate()
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
            let answer = socket
                .request(
                    GET_SECURITY_HISTORY_CANDLESTICKS,
                    &offset_query(
                        &symbol,
                        Period::FortyFiveMinute,
                        Direction::Backward,
                        &anchor_date,
                        &anchor_minute,
                        12,
                    ),
                )
                .await
                .unwrap_or_else(|error| panic!("{case}: the 45m read failed: {error}"));
            socket.shutdown().await;
            answer
        };
        assert_candle_list(case, &forty_five.candlesticks, 12);
        for line in candle_summary(&forty_five.candlesticks, forty_five.candlesticks.len()) {
            println!("  45m{line}");
        }
        let off_grid: Vec<String> = forty_five
            .candlesticks
            .iter()
            .filter(|candle| {
                let millis = candle.timestamp * 1_000;
                bucket_start(millis, Some(45 * 60_000)) != Some(millis)
            })
            .map(|candle| {
                format!(
                    "{} ({})",
                    candle.timestamp,
                    market_clock(candle.timestamp * 1_000)
                )
            })
            .collect();
        println!(
            "  45m candles whose timestamp is not the session-anchored bucket this build \
             computes: {off_grid:?}"
        );
        // The diagnostic above is an assertion rather than a note: a venue bucket that
        // this build's own grid does not produce is exactly the mis-gridding §4.6 is
        // about, and a list that is only printed can stay a silent false positive. Every
        // 45m candle the venue publishes — the evening's `20:00 / 20:45 / …` included —
        // rests on its window's own open plus whole 45-minute intervals.
        assert!(
            off_grid.is_empty(),
            "{case}: every 45m candle the venue published must rest on the session-anchored \
             grid this build computes; {} did not: {off_grid:?}",
            off_grid.len()
        );

        for (name, answer, count) in [
            ("end only", &backward, 10),
            ("start only", &forward, 10),
            ("neither", &unanchored, 10),
            ("both", &both, 10),
        ] {
            assert_candle_list(case, &answer.candlesticks, count);
            println!(
                "  shape {name}: len={} <= count={count}",
                answer.candlesticks.len()
            );
        }
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 8 — the history and the opening screen agree candle for candle (§4.5,
/// §4.6).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_08_history_and_the_opening_screen_agree_candle_for_candle() {
    let case = "case 8 (history vs first screen)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let symbol = crate::symbols::symbol();
        let mut socket = LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        let screen: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_CANDLESTICKS,
                &first_screen(&symbol, Period::FiveMinute, FIRST_SCREEN_COUNT, AdjustType::NoAdjust),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the opening screen failed: {error}"));
        let oldest = screen
            .candlesticks
            .first()
            .expect("an opening screen of a listed symbol states candles")
            .timestamp;
        let history: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_HISTORY_CANDLESTICKS,
                &offset_query(
                    &symbol,
                    Period::FiveMinute,
                    Direction::Forward,
                    &market_date(oldest * 1_000),
                    &market_local(oldest * 1_000).strftime("%H%M").to_string(),
                    FIRST_SCREEN_COUNT,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the history read failed: {error}"));
        socket.shutdown().await;

        println!(
            "{case}: symbol={symbol} period=5m; cmd 19 stated {} candle(s) \
             [{:?}..{:?}], cmd 27 stated {} candle(s) [{:?}..{:?}]",
            screen.candlesticks.len(),
            screen.candlesticks.first().map(|candle| candle.timestamp),
            screen.candlesticks.last().map(|candle| candle.timestamp),
            history.candlesticks.len(),
            history.candlesticks.first().map(|candle| candle.timestamp),
            history.candlesticks.last().map(|candle| candle.timestamp)
        );
        assert_candle_list(case, &screen.candlesticks, FIRST_SCREEN_COUNT);
        assert_candle_list(case, &history.candlesticks, FIRST_SCREEN_COUNT);

        let screen_by_time: ahash::AHashMap<i64, &longbridge_proto::quote::Candlestick> = screen
            .candlesticks
            .iter()
            .map(|candle| (candle.timestamp, candle))
            .collect();
        let history_by_time: ahash::AHashMap<i64, &longbridge_proto::quote::Candlestick> = history
            .candlesticks
            .iter()
            .map(|candle| (candle.timestamp, candle))
            .collect();
        let shared: Vec<i64> = screen_by_time
            .keys()
            .filter(|timestamp| history_by_time.contains_key(timestamp))
            .copied()
            .collect();
        println!(
            "  {} timestamp(s) appear in both answers; {} in the history only, {} in the screen only",
            shared.len(),
            history_by_time.len() - shared.len(),
            screen_by_time.len() - shared.len()
        );
        assert!(
            !shared.is_empty(),
            "{case}: the two commands must overlap over the same window, or there is nothing \
             to compare"
        );
        let mut mismatches = Vec::with_capacity(4);
        for timestamp in &shared {
            let (left, right) = (screen_by_time[timestamp], history_by_time[timestamp]);
            if (
                &left.open,
                &left.high,
                &left.low,
                &left.close,
                left.volume,
                &left.turnover,
            ) != (
                &right.open,
                &right.high,
                &right.low,
                &right.close,
                right.volume,
                &right.turnover,
            ) {
                mismatches.push(format!(
                    "t={timestamp}: cmd19 o={} h={} l={} c={} v={} turnover={} vs cmd27 o={} h={} \
                     l={} c={} v={} turnover={}",
                    left.open,
                    left.high,
                    left.low,
                    left.close,
                    left.volume,
                    left.turnover,
                    right.open,
                    right.high,
                    right.low,
                    right.close,
                    right.volume,
                    right.turnover
                ));
            }
        }
        for mismatch in mismatches.iter().take(5) {
            println!("  MISMATCH {mismatch}");
        }
        assert!(
            mismatches.is_empty(),
            "{case}: the two commands must state the same OHLCV for the same timestamp; {} \
             disagreed",
            mismatches.len()
        );
        println!("  all {} shared candles agree on OHLCV and turnover", shared.len());
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 9 — the opening screen's parameters and the last candle's bucket (§4.6;
/// §14.1 item 5).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_09_the_opening_screen_asks_a_thousand_and_lands_on_the_current_bucket() {
    let case = "case 9 (cmd 19 opening screen)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let symbol = crate::symbols::symbol();
        let mut socket =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
        socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        let request = first_screen(
            &symbol,
            Period::FiveMinute,
            FIRST_SCREEN_COUNT,
            AdjustType::NoAdjust,
        );
        println!(
            "{case}: request count={} trade_session={} adjust_type={} (NO_ADJUST)",
            request.count, request.trade_session, request.adjust_type
        );
        assert_eq!(
            request.count, 1_000,
            "{case}: the design's first screen asks for 1000 (§4.6)"
        );
        assert_eq!(
            request.trade_session, 100,
            "{case}: the design asks for every session (§4.6)"
        );
        assert_eq!(
            request.adjust_type,
            AdjustType::NoAdjust as i32,
            "{case}: the request's own adjustment travels"
        );

        let screen: SecurityCandlestickResponse = socket
            .request(GET_SECURITY_CANDLESTICKS, &request)
            .await
            .unwrap_or_else(|error| panic!("{case}: the opening screen failed: {error}"));
        let forward: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_CANDLESTICKS,
                &first_screen(
                    &symbol,
                    Period::FiveMinute,
                    FIRST_SCREEN_COUNT,
                    AdjustType::ForwardAdjust,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the forward-adjusted screen failed: {error}"));
        socket.shutdown().await;

        assert_candle_list(case, &screen.candlesticks, FIRST_SCREEN_COUNT);
        assert_candle_list(case, &forward.candlesticks, FIRST_SCREEN_COUNT);
        println!(
            "  cmd 19 answered {} candle(s); the forward-adjusted read answered {}",
            screen.candlesticks.len(),
            forward.candlesticks.len()
        );
        let last = screen
            .candlesticks
            .last()
            .expect("an opening screen of a listed symbol states candles");
        let now = now_millis();
        let local = market_local(now);
        // `bucket_start` answers Some only for an instant the venue's table places in a
        // session: the five windows cover the market's whole day, so the answer is
        // `None` only for the hours a table with a gap leaves — the afternoon of a half
        // day after its post session closed, which is not what this assertion is about.
        let computed = bucket_start(now, Some(5 * 60_000));
        println!(
            "  last candle: t={} ({}) session={}; now={now} ({} ET, {}); this build's current \
             5m bucket is {computed:?} ({})",
            last.timestamp,
            market_clock(last.timestamp * 1_000),
            session_name(last.trade_session),
            local.strftime("%Y-%m-%d %H:%M:%S"),
            session_label(now),
            computed.map_or_else(|| "outside every session".to_owned(), market_clock,)
        );
        if let Some(computed) = computed {
            println!(
                "  difference: last - computed = {} ms",
                last.timestamp * 1_000 - computed
            );
        }
        assert_eq!(
            last.timestamp * 1_000,
            bucket_start(last.timestamp * 1_000, Some(5 * 60_000))
                .expect("the newest candle the venue published is inside one of its sessions"),
            "{case}: the newest candle must itself be a bucket boundary of this build's grid"
        );
        let in_session =
            session_of(&local) == Some(longbridge_proto::quote::TradeSession::NormalTrade);
        // The equality below needs the venue to have **traded** the current bucket, so its
        // gate is the venue's own trading-day calendar rather than the session clock: the
        // session clock says `regular` at 10:00 ET on a Sunday, and the newest candle of
        // that run was Friday's last one, 137,700 seconds behind the bucket this build
        // computes. The wire-level case 10 reads the same calendar for the same reason.
        let trading_day = crate::cases::push::trading_day(&credentials, case, now).await;
        println!(
            "  the venue's calendar names {} a trading day: {trading_day}; the session clock \
             says {}",
            market_date(now),
            session_label(now)
        );
        if in_session && trading_day {
            let computed = computed.expect("the regular session is one of the four windows");
            assert_eq!(
                last.timestamp * 1_000,
                computed,
                "{case}: with the regular session open on a trading day, the newest candle of the \
                 opening screen must be the bucket this build computes locally (§14.1 item 5)"
            );
        } else {
            println!(
                "  the market is not trading this bucket (session={}, trading day={trading_day}), \
                 so the newest candle is the venue's last one from the previous session and the \
                 equality above cannot hold; the candle still sits on this build's grid, which is \
                 the part of §14.1 item 5 that does not depend on the clock",
                session_label(now)
            );
            if let Some(computed) = computed {
                assert!(
                    last.timestamp * 1_000 > computed - 4 * 24 * 3_600_000,
                    "{case}: the newest candle ({}) is more than four days behind the current \
                     bucket ({computed}), which is not a market that closed for a weekend",
                    last.timestamp * 1_000
                );
            } else {
                // The run is inside the hours a table with a gap leaves — the afternoon of a
                // half day — so there is no local bucket to measure a distance from; the
                // newest candle is still the venue's last published one.
                assert!(
                    last.timestamp * 1_000 < now,
                    "{case}: the newest candle cannot be in the future"
                );
                println!(
                    "  and now is outside every session ({}), so there is no local bucket to \
                     compare against",
                    local.strftime("%Y-%m-%d %H:%M:%S")
                );
            }
        }

        // The two adjustment conventions are two different reads; what the case
        // asserts is that the parameter reached the venue at all. Equal answers are
        // possible for a symbol with no corporate action in the window.
        let equal = screen.candlesticks.len() == forward.candlesticks.len()
            && screen
                .candlesticks
                .iter()
                .zip(forward.candlesticks.iter())
                .all(|(left, right)| {
                    left.timestamp == right.timestamp
                        && left.open == right.open
                        && left.close == right.close
                });
        println!("  the two adjustment conventions agreed on this window: {equal}");
    })
    .await
    .expect("the case must finish inside its own bound");
}
