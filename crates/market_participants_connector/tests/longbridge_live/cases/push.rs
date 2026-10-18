//! The push cases: the subscription's two pushes, the day gate, and the half-day grid
//! (§4.6; §14.1 items 2, 4, 5).

use ahash::AHashSet;
use jiff::civil;
use longbridge_proto::quote::{
    AdjustType, Candlestick, Direction, MarketTradeDayRequest, MarketTradeDayResponse, Period,
    PushQuote, PushQuoteTag, PushTrade, SecurityCandlestickResponse, SubType, SubscribeRequest,
    TradeSession, UnsubscribeRequest,
};
use prost::Message as _;
use tokio::time::timeout;

use crate::{
    bounds::{CASE_TIMEOUT, FIRST_SCREEN_COUNT, PUSH_WINDOW},
    client::chosen_transport,
    credentials::Credentials,
    describe::{
        assert_candle_list, assert_quote_snapshot, in_regular_session, market_clock, session_label,
        session_name,
    },
    grid::{
        bucket_start, half_day_bucket_start, half_day_session_of, market_date, market_instant,
        market_local, now_millis, session_of,
    },
    skip_without_credentials,
    socket::LiveSocket,
    wire::{
        Failure, GET_SECURITY_CANDLESTICKS, GET_SECURITY_HISTORY_CANDLESTICKS, GET_TRADING_DAYS,
        HEADER_GZIP, HEADER_TYPE_MASK, HEADER_VERIFY, PUSH_REALTIME_QUOTE, PUSH_REALTIME_TRADES,
        SUBSCRIBE, SUBSCRIPTION_REFUSAL, UNSUBSCRIBE,
    },
};

/// Reads the venue's own trading days from `beg_day` forward (cmd 9), narrowed until the
/// venue accepts the window.
///
/// The venue refuses a window it calls too many days (`301600 too many query days`) and
/// does not state the bound, so the range starts at a season's width and halves until an
/// answer arrives; a refusal of every width stops the case rather than guessing. What is
/// returned is `trade_day`, which is the venue's own statement of which days trade.
async fn trading_days(socket: &mut LiveSocket, case: &str, beg_day: &str) -> Vec<String> {
    let start = civil::Date::strptime("%Y%m%d", beg_day).expect("a date this suite formatted");
    let mut refused: Vec<String> = Vec::with_capacity(4);
    for days in [30_i64, 20, 10, 7] {
        let end = start
            .checked_add(jiff::Span::new().days(days))
            .expect("a date inside the calendar");
        let request = MarketTradeDayRequest {
            market: "US".to_owned(),
            beg_day: beg_day.to_owned(),
            end_day: end.strftime("%Y%m%d").to_string(),
        };
        match socket.request(GET_TRADING_DAYS, &request).await {
            Ok(answer) => {
                let answer: MarketTradeDayResponse = answer;
                return answer.trade_day;
            }
            Err(Failure::Refused { code, message, .. }) => {
                refused.push(format!("{days} days: {code} {message}"));
            }
            Err(error) => panic!("{case}: the trading-day read failed: {error}"),
        }
    }
    panic!("{case}: the venue refused every trading-day window this case tried: {refused:?}")
}

/// Reports whether the venue's own calendar names one instant's market day a trading day.
///
/// The session clock cannot answer this: 09:30–16:00 ET on a Sunday is a session the venue
/// states and a day it does not trade, and the two claims are the venue's own. A case that
/// needs "the market is trading now" — the opening screen's current bucket, the `104`
/// clause — reads the calendar here rather than assuming it from the clock.
pub async fn trading_day(credentials: &Credentials, case: &str, millis: i64) -> bool {
    let day = market_date(millis);
    let mut socket = LiveSocket::connect(credentials, chosen_transport(credentials).await, true)
        .await
        .unwrap_or_else(|error| panic!("{case}: the calendar's socket failed: {error}"));
    socket
        .authenticate()
        .await
        .unwrap_or_else(|error| panic!("{case}: the calendar's AUTH failed: {error}"));
    let readings = trading_days(&mut socket, case, &day).await;
    socket.shutdown().await;
    readings.contains(&day)
}

#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_10_a_subscription_pushes_quotes_and_trades() {
    let case = "case 10 (subscribe and push)";
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

        let screen: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_CANDLESTICKS,
                &crate::cases::stream::first_screen(
                    &symbol,
                    Period::OneMinute,
                    FIRST_SCREEN_COUNT,
                    AdjustType::NoAdjust,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the opening screen failed: {error}"));
        assert_candle_list(case, &screen.candlesticks, FIRST_SCREEN_COUNT);
        let screen_last = screen
            .candlesticks
            .last()
            .map(|candle| candle.timestamp * 1_000);
        println!(
            "{case}: symbol={symbol}; the opening screen stated {} candle(s), newest {:?}",
            screen.candlesticks.len(),
            screen_last
        );

        // The venue's own calendar, read on this socket before the subscription: whether
        // today trades is what decides whether the `104` clause can be judged at all, and
        // the session clock cannot answer it — 09:30–16:00 ET on a Sunday is a session
        // with no prints.
        let readings = trading_days(&mut socket, case, &market_date(now_millis())).await;
        println!(
            "{case}: the venue's calendar states {} trade day(s) from {}; half days: {}",
            readings.len(),
            market_date(now_millis()),
            readings.is_empty()
        );

        let subscribe = SubscribeRequest {
            symbol: vec![symbol.clone()],
            sub_type: vec![SubType::Quote as i32, SubType::Trade as i32],
            is_first_push: true,
        };
        let answer = socket
            .request_raw(SUBSCRIBE, &subscribe.encode_to_vec())
            .await;
        let answer = match answer {
            Ok(answer) => answer,
            Err(Failure::Refused { code, message, .. }) => {
                panic!("{case}: {SUBSCRIPTION_REFUSAL} (code {code}): {message}")
            }
            Err(error) => panic!("{case}: {error}"),
        };
        println!(
            "  cmd 6 ack: command_code={} status={:?} body_len={}",
            answer.command_code,
            answer.status,
            answer.body.len()
        );
        assert_eq!(answer.command_code, SUBSCRIBE);
        assert_eq!(answer.status, Some(0), "{case}: the venue accepted cmd 6");

        let deadline = tokio::time::Instant::now() + PUSH_WINDOW;
        let mut quotes = Vec::with_capacity(4);
        let mut trades = Vec::with_capacity(4);
        let mut others: Vec<u8> = Vec::with_capacity(4);
        let recorded = socket.drain_pushes(deadline).await;
        let frames = recorded.len();
        for push in recorded {
            println!(
                "  push: command={} frame_len={} declared_body_len={}",
                push.command_code,
                push.frame.len(),
                push.declared_body_len
            );
            match push.command_code {
                PUSH_REALTIME_QUOTE => match PushQuote::decode(push.body.as_ref()) {
                    Ok(quote) => quotes.push(quote),
                    Err(error) => println!("  a 101 frame did not decode: {error}"),
                },
                PUSH_REALTIME_TRADES => match PushTrade::decode(push.body.as_ref()) {
                    Ok(trade) => trades.push(trade),
                    Err(error) => println!("  a 104 frame did not decode: {error}"),
                },
                other => others.push(other),
            }
        }
        let observed = PUSH_WINDOW
            .saturating_sub(deadline.saturating_duration_since(tokio::time::Instant::now()));
        println!("  every inbound frame of this socket, in order:");
        for (millis, header, command, request_id, length) in &socket.trace {
            println!(
                "    +{millis} ms header={header:#04x} (type={} gzip={} verify={}) command={command} \
                 request_id={request_id:?} len={length}",
                header & HEADER_TYPE_MASK,
                header & HEADER_GZIP != 0,
                header & HEADER_VERIFY != 0
            );
        }
        println!(
            "  over {observed:?}: {frames} inbound push frame(s): {} quote(s), {} trade(s), other \
             command codes {:?}",
            quotes.len(),
            trades.len(),
            others
        );
        for quote in quotes.iter().take(3) {
            println!(
                "    101 {} t={} ({}) session={} tag={} last={} volume={} current_volume={} \
                 trade_status={}",
                quote.symbol,
                quote.timestamp,
                market_clock(quote.timestamp.saturating_mul(1_000)),
                session_name(quote.trade_session),
                quote.tag,
                quote.last_done,
                quote.volume,
                quote.current_volume,
                quote.trade_status
            );
        }
        for trade in trades.iter().take(2) {
            println!(
                "    104 {} sequence={} prints={}",
                trade.symbol,
                trade.sequence,
                trade.trade.len()
            );
            for print in trade.trade.iter().take(3) {
                println!(
                    "      print price={} volume={} t={} ({}) session={} type={:?} direction={:?}",
                    print.price,
                    print.volume,
                    print.timestamp,
                    market_clock(print.timestamp.saturating_mul(1_000)),
                    session_name(print.trade_session),
                    print.trade_type,
                    print.direction
                );
            }
        }

        assert!(
            quotes.iter().all(|quote| quote.symbol == symbol),
            "{case}: every push must be about the symbol that was subscribed"
        );
        assert!(
            trades.iter().all(|trade| trade.symbol == symbol),
            "{case}: every push must be about the symbol that was subscribed"
        );
        for quote in &quotes {
            assert_quote_snapshot(case, quote, &symbol);
        }

        // The one assertion this case cannot make from a closed market, and the reason it
        // states which situation it measured: `QUOTE | TRADE` with `is_first_push` answers a
        // `104` only where the venue has trades to push. Three runs measured three
        // situations — a closed market on a Saturday evening (one `101`, `session=post`),
        // the overnight session on a Sunday morning (the same single `101`), and **a
        // weekend inside the regular session's own clock** (Sunday 10:00 ET): the last one
        // is why the gate is the venue's *calendar* rather than its session clock, because
        // 09:30–16:00 ET on a day the venue does not trade is a session with no prints.
        let now = now_millis();
        let trading_day = readings.contains(&market_date(now));
        println!(
            "  the 104 clause: now is {} ET ({}), today is a trading day the venue states: \
             {trading_day}",
            market_local(now).strftime("%Y-%m-%d %H:%M:%S"),
            session_label(now)
        );
        if trading_day {
            assert!(
                !trades.is_empty(),
                "{case}: the venue states that {} is a trading day and the market is {} now, so \
                 a subscription with is_first_push must answer with a 104 trade push (§4.6); it \
                 answered {} quote(s) and no trades",
                market_date(now),
                session_label(now),
                quotes.len()
            );
            println!(
                "  the 104 assertion was judged and passed on the venue's own trading day {}: {} \
                 trade push(es) arrived",
                market_date(now),
                trades.len()
            );
        } else {
            println!(
                "  SKIP the 104 assertion: the venue's calendar does not name {} a trading day, \
                 so there are no prints to push and the trade clause CANNOT BE JUDGED at {} ET \
                 (the session clock says {}). This case is not passing it — it asserts the 101 \
                 snapshot's own shape instead, and the live run must be repeated on a trading \
                 day for §14.1 item 2's subscription clause to have an answer.",
                market_date(now),
                market_local(now).strftime("%Y-%m-%d %H:%M:%S"),
                session_label(now)
            );
            println!(
                "  measured on a non-trading day: {} snapshot(s), {} trade push(es) in {:?}",
                quotes.len(),
                trades.len(),
                PUSH_WINDOW
            );
        }

        // The candle rule the design states, applied to what actually arrived: a
        // quote push only merges inside the regular session, a daily candle only
        // exists inside the regular session, and a frame outside every session makes
        // no candle at all.
        let mut minute_buckets: Vec<i64> = Vec::with_capacity(quotes.len());
        let mut day_buckets: AHashSet<i64> = AHashSet::with_capacity(2);
        let mut eod_frames = 0_usize;
        let mut eod_candles = 0_usize;
        for quote in &quotes {
            let millis = quote.timestamp.saturating_mul(1_000);
            let session = session_of(&market_local(millis));
            let produces = session == Some(TradeSession::NormalTrade);
            println!(
                "    merge check: t={millis} session={} tag={} -> quote push {} a candle",
                session_label(millis),
                quote.tag,
                if produces { "makes" } else { "makes no" }
            );
            if produces {
                minute_buckets.extend(bucket_start(millis, Some(60_000)));
                day_buckets.extend(bucket_start(millis, None));
            }
            if quote.tag == PushQuoteTag::Eod as i32 {
                eod_frames += 1;
                // The design states that an EOD frame still runs the merge (§4.6):
                // it can update the day's candle or roll it. What it may not do is
                // produce a *new* day bucket outside the regular session.
                let would_be = produces;
                println!(
                    "      EOD frame observed: session={} -> {} a candle",
                    session_label(millis),
                    if would_be { "makes" } else { "makes no" }
                );
                if !would_be {
                    eod_candles += 1;
                }
            }
        }
        println!(
            "  candles the pushes produce under §4.6's gates: {} distinct 1m bucket(s), {} \
             distinct DAY bucket(s); {} EOD frame(s), of which {} would produce a candle",
            minute_buckets.len(),
            day_buckets.len(),
            eod_frames,
            eod_candles
        );
        let quotes_in_session = quotes
            .iter()
            .filter(|quote| {
                session_of(&market_local(quote.timestamp.saturating_mul(1_000)))
                    == Some(TradeSession::NormalTrade)
            })
            .count();
        assert!(
            quotes_in_session == 0 || !minute_buckets.is_empty(),
            "{case}: every quote that arrived in the regular session must produce a 1m candle \
             (§4.6), and {quotes_in_session} did"
        );
        if trading_day {
            // The candle the trade push opens is the other half of the in-session half of
            // this case: a `104` inside the regular session reaches that session's own 1m
            // bucket, which is the rule the aggregator applies. Its gate is the venue's
            // calendar: without a trading day there are no prints to bucket at all.
            let trades_in_session = trades
                .iter()
                .filter(|trade| {
                    trade.trade.iter().any(|print| {
                        in_regular_session(print.timestamp.saturating_mul(1_000))
                    })
                })
                .count();
            let trade_buckets: AHashSet<i64> = trades
                .iter()
                .flat_map(|trade| trade.trade.iter())
                .filter(|print| in_regular_session(print.timestamp.saturating_mul(1_000)))
                .filter_map(|print| bucket_start(print.timestamp.saturating_mul(1_000), Some(60_000)))
                .collect();
            assert!(
                trades_in_session > 0 && !trade_buckets.is_empty(),
                "{case}: a 104 on a trading day must reach a 1m bucket of that session, \
                 and {trades_in_session} of {} did",
                trades.len()
            );
            println!(
                "  §4.6: {trades_in_session} trade push(es) in the regular session reach {} \
                 distinct 1m bucket(s)",
                trade_buckets.len()
            );
        }
        assert_eq!(
            eod_candles, 0,
            "{case}: an EOD frame outside the regular session must not produce a candle (§4.6)"
        );
        if eod_frames == 0 {
            println!(
                "  the venue stated no EOD frame in this window, so the EOD clause above was \
                 not exercised (the market is {} at {} ET)",
                if in_regular_session(now_millis()) {
                    "open"
                } else {
                    "closed"
                },
                market_local(now_millis()).strftime("%Y-%m-%d %H:%M:%S")
            );
        }

        let unsubscribe = UnsubscribeRequest {
            symbol: vec![symbol.clone()],
            sub_type: vec![SubType::Quote as i32, SubType::Trade as i32],
            unsub_all: false,
        };
        let answer = socket
            .request_raw(UNSUBSCRIBE, &unsubscribe.encode_to_vec())
            .await;
        match answer {
            Ok(answer) => println!(
                "  cmd 7 ack: command_code={} status={:?}",
                answer.command_code, answer.status
            ),
            Err(error) => println!("  cmd 7 was not acknowledged: {error}"),
        }
        socket.shutdown().await;
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 12 — the half-day calendar (cmd 9; §4.6; §14.1 item 4).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_12_the_half_day_calendar_and_a_half_days_post_session() {
    let case = "case 12 (cmd 9 half days)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let today = market_date(now_millis());
        let today_date = civil::Date::strptime("%Y%m%d", &today).expect("a date this suite read");
        let mut socket =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
        socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        // The venue refuses a window it calls too many days (`301600 too many query
        // days`), and it does not say what the bound is; the range starts at the
        // season's own width and narrows until the venue answers.
        let mut calendar: Option<MarketTradeDayResponse> = None;
        let mut refused: Vec<String> = Vec::with_capacity(3);
        for days in [90_i64, 45, 20, 7] {
            let end = today_date
                .checked_add(jiff::Span::new().days(days))
                .expect("a date inside the calendar");
            let request = MarketTradeDayRequest {
                market: "US".to_owned(),
                beg_day: today.clone(),
                end_day: end.strftime("%Y%m%d").to_string(),
            };
            println!(
                "{case}: today is {today}; asking for {}..{} ({days} days)",
                request.beg_day, request.end_day
            );
            match socket.request(GET_TRADING_DAYS, &request).await {
                Ok(answer) => {
                    calendar = Some(answer);
                    break;
                }
                Err(Failure::Refused { code, message, .. }) => {
                    println!("  the venue refused a {days}-day window with code {code}: {message}");
                    refused.push(format!("{days} days: {code} {message}"));
                }
                Err(error) => panic!("{case}: the trading-day read failed: {error}"),
            }
        }
        let calendar = calendar.unwrap_or_else(|| {
            panic!("{case}: the venue refused every window this case tried: {refused:?}")
        });

        println!(
            "{case}: the venue answered {} trade day(s) and {} half trade day(s)",
            calendar.trade_day.len(),
            calendar.half_trade_day.len()
        );
        println!("  half_trade_day = {:?}", calendar.half_trade_day);
        println!(
            "  first trade days = {:?}",
            &calendar.trade_day[..calendar.trade_day.len().min(5)]
        );
        assert!(
            !calendar.trade_day.is_empty(),
            "{case}: a season of US trading days is not empty"
        );
        assert!(
            calendar
                .trade_day
                .iter()
                .all(|day| day.len() == 8 && day.chars().all(|c| c.is_ascii_digit())),
            "{case}: a trading day is stated as YYYYMMDD"
        );
        assert!(
            calendar
                .half_trade_day
                .iter()
                .all(|day| calendar.trade_day.contains(day)),
            "{case}: every half day is also a trading day"
        );

        let today_is_half = calendar.half_trade_day.contains(&today);
        println!("  today ({today}) is a half trading day: {today_is_half}");
        // The most recent half day the venue's own calendar states. Today's own
        // session is only observable while today *is* a half day; on any other day the
        // only way to see a half day's grid is to read the most recent one back.
        let probe = calendar
            .half_trade_day
            .iter()
            .rev()
            .find(|day| day.as_str() <= today.as_str())
            .cloned()
            .unwrap_or_default();
        if probe.is_empty() {
            println!(
                "  MEASUREMENT NOT TAKEN: the venue's calendar states no half day at or before \
                 {today}, so the post session's 13:00 grid could not be observed"
            );
            socket.shutdown().await;
            return;
        }
        println!(
            "  probing the half day {probe} (today is {today}; this is a read of that day's own \
             1m candles)"
        );
        // The half day's own candles, read back from its date: a date query would
        // return as many candles as the venue decides, so the anchored offset query
        // is used with the day's own closing minute as the anchor.
        let candles: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_HISTORY_CANDLESTICKS,
                &crate::cases::stream::offset_query(
                    &crate::symbols::symbol(),
                    Period::OneMinute,
                    Direction::Backward,
                    &probe,
                    "1300",
                    FIRST_SCREEN_COUNT,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the 1m read of {probe} failed: {error}"));
        socket.shutdown().await;

        let post: Vec<&Candlestick> = candles
            .candlesticks
            .iter()
            .filter(|candle| {
                let local = market_local(candle.timestamp * 1_000);
                market_date(candle.timestamp * 1_000) == probe
                    && half_day_session_of(&local) == Some(TradeSession::PostTrade)
            })
            .collect();
        let regular: Vec<&Candlestick> = candles
            .candlesticks
            .iter()
            .filter(|candle| {
                let local = market_local(candle.timestamp * 1_000);
                market_date(candle.timestamp * 1_000) == probe
                    && half_day_session_of(&local) == Some(TradeSession::NormalTrade)
            })
            .collect();
        println!(
            "  the 1m screen holds {} candle(s) of {probe}: {} regular, {} post",
            candles
                .candlesticks
                .iter()
                .filter(|candle| market_date(candle.timestamp * 1_000) == probe)
                .count(),
            regular.len(),
            post.len()
        );
        if post.is_empty() {
            println!(
                "  MEASUREMENT NOT TAKEN: the venue stated no post-session candle for {probe}, so \
                 that half day's 13:00 grid could not be observed"
            );
            return;
        }
        let first_post = post
            .iter()
            .map(|candle| candle.timestamp * 1_000)
            .min()
            .expect("a non-empty post session");
        let expected = market_instant(&probe, 13, 0);
        println!(
            "  the half day {probe} post session starts at {} and this build's half-day table \
             starts it at {expected} ({}); today is {today}, which is a half day: {today_is_half}",
            market_clock(first_post),
            market_clock(expected)
        );
        // Every post candle of that day rests on the half-day table's own grid, and the
        // 15:00 one is the discriminating case: an ordinary day's regular session would
        // have put it on the 09:30 grid, and no grid at all would have dropped it.
        let grid: Vec<(i64, Option<i64>)> = post
            .iter()
            .map(|candle| {
                let millis = candle.timestamp * 1_000;
                (millis, half_day_bucket_start(millis, Some(60_000)))
            })
            .collect();
        let off_grid: Vec<i64> = grid
            .iter()
            .filter(|(millis, bucket)| *bucket != Some(*millis))
            .map(|(millis, _)| *millis)
            .collect();
        println!(
            "  every post candle of {probe} rests on the half-day grid: {} checked, {off_grid:?} \
             off it",
            grid.len()
        );
        assert!(
            off_grid.is_empty(),
            "{case}: every post-session 1m candle of a half day must be a bucket of this build's \
             half-day table; {off_grid:?} are not"
        );
        if let Some(candle) = post
            .iter()
            .find(|candle| market_clock(candle.timestamp * 1_000) == "15:00")
        {
            assert_ne!(
                half_day_bucket_start(candle.timestamp * 1_000, Some(60_000)),
                bucket_start(candle.timestamp * 1_000, Some(60_000)),
                "{case}: 15:00 belongs to the half day's post grid, not to an ordinary day's \
                 regular one — which is the whole reason the table is replaced rather than \
                 shortened"
            );
        }
        println!(
            "  the half day's regular session holds {} candle(s) and its post session {} \
             candle(s), so the whole day's grid is the replaced table's",
            regular.len(),
            post.len()
        );
        assert_eq!(
            first_post, expected,
            "{case}: a half day's post session starts at 13:00, which is the half-day table's own \
             open (§4.6, decision 14)"
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}
