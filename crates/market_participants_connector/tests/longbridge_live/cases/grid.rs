//! The day-gate case: what a daily candle is bounded by, and what a non-regular print
//! may reach (§4.6; §14.1 item 5).
//!
//! The session table this case checks against is [`crate::grid`]'s — the one table the
//! suite states — so a change to the venue's windows shows up here as a failing
//! assertion rather than as a diagnostic nobody reads.

use ahash::{AHashMap, AHashSet};
use longbridge_proto::quote::{
    AdjustType, Direction, Period, SecurityCandlestickResponse, TradeSession,
};
use tokio::time::timeout;

use crate::{
    bounds::{CASE_TIMEOUT, FIRST_SCREEN_COUNT},
    client::chosen_transport,
    credentials::Credentials,
    describe::{assert_candle_list, bucket_start, market_clock, session_label, session_name},
    grid::{
        half_day_session_of, market_date, market_instant, market_local, now_millis, session_of,
    },
    skip_without_credentials,
    socket::LiveSocket,
    wire::{GET_SECURITY_CANDLESTICKS, GET_SECURITY_HISTORY_CANDLESTICKS},
};
/// Case 11 — the day gate: a `DAY` candle's boundary, and what a non-regular
/// session may not produce (§4.6; §14.1 item 5).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_11_a_day_candle_starts_at_the_markets_midnight() {
    let case = "case 11 (DAY gate)";
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
        let days: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_HISTORY_CANDLESTICKS,
                &crate::cases::stream::offset_query(&symbol, Period::Day, Direction::Backward, "", "", 10),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the DAY read failed: {error}"));
        let minutes: SecurityCandlestickResponse = socket
            .request(
                GET_SECURITY_CANDLESTICKS,
                &crate::cases::stream::first_screen(&symbol, Period::OneMinute, FIRST_SCREEN_COUNT, AdjustType::NoAdjust),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: the 1m read failed: {error}"));
        socket.shutdown().await;

        assert_candle_list(case, &days.candlesticks, 10);
        println!("{case}: symbol={symbol}; {} DAY candle(s)", days.candlesticks.len());
        let mut off_midnight = Vec::with_capacity(4);
        for candle in &days.candlesticks {
            let millis = candle.timestamp * 1_000;
            let local = market_local(millis);
            let midnight = bucket_start(millis, None)
                .expect("a DAY candle the venue published is inside one of its windows");
            println!(
                "    t={} -> {} {} ET (session={}) day bucket={} ({})",
                candle.timestamp,
                local.strftime("%Y-%m-%d"),
                local.strftime("%H:%M:%S"),
                session_name(candle.trade_session),
                midnight,
                market_clock(midnight)
            );
            if millis != midnight {
                off_midnight.push(candle.timestamp);
            }
        }
        assert!(
            days.candlesticks
                .iter()
                .all(|candle| session_name(candle.trade_session) == "normal"),
            "{case}: a DAY candle belongs to the regular session (§4.6)"
        );
        assert!(
            off_midnight.is_empty(),
            "{case}: a DAY candle's timestamp must be the market's local midnight; {off_midnight:?} \
             are not"
        );
        println!(
            "  all {} DAY candles start at the market's local midnight",
            days.candlesticks.len()
        );

        // Non-regular prints and the buckets they may reach: a pre or post print
        // may only reach a minute bucket, and the day bucket it would fall in is
        // reported so a reader can see that it is the same day the venue published.
        let mut non_regular: AHashMap<String, usize> = AHashMap::with_capacity(3);
        let mut pre_post_minute_buckets: AHashSet<i64> = AHashSet::with_capacity(16);
        let published_days: AHashSet<i64> = days
            .candlesticks
            .iter()
            .map(|candle| candle.timestamp * 1_000)
            .collect();
        for candle in &minutes.candlesticks {
            let millis = candle.timestamp * 1_000;
            let session = session_of(&market_local(millis));
            if session == Some(TradeSession::NormalTrade) {
                continue;
            }
            *non_regular
                .entry(session_label(millis).to_owned())
                .or_insert(0) += 1;
            pre_post_minute_buckets.extend(bucket_start(millis, Some(60_000)));
        }
        println!(
            "  the opening screen holds {} non-regular 1m candle(s) {:?}, reaching {} distinct \
             1m bucket(s); the DAY buckets published for the same days: {}",
            non_regular.values().sum::<usize>(),
            non_regular,
            pre_post_minute_buckets.len(),
            published_days.len()
        );
        assert!(
            non_regular
                .keys()
                .all(|session| session == "pre" || session == "post" || session == "overnight"),
            "{case}: the non-regular candles of a US equity are its pre, post and overnight \
             candles"
        );

        // The overnight session is the one window the venue's own series states (§4.6,
        // §4.8), asserted rather than described: it opens at 20:00 and runs to 04:00 of
        // the next morning, crossing the market's midnight, and the venue's own 45-minute
        // series is what says so — `23:45` is followed by `00:30` on the same 2700-second
        // grid. The half-day table's own gap is the control: 17:30 of a half day is between
        // its post session's 17:00 close and the evening window's 20:00 open, so it is
        // inside no session at all.
        let today = market_date(now_millis());
        for (hour, ordinary, half_day) in [
            (2, Some(TradeSession::OvernightTrade), Some(TradeSession::OvernightTrade)),
            (4, Some(TradeSession::PreTrade), Some(TradeSession::PreTrade)),
            (17, Some(TradeSession::PostTrade), None),
            (21, Some(TradeSession::OvernightTrade), Some(TradeSession::OvernightTrade)),
        ] {
            let instant = market_instant(&today, hour, 0);
            let local = market_local(instant);
            println!(
                "  {hour:02}:00 ET -> ordinary table {:?}, half-day table {:?}",
                session_of(&local),
                half_day_session_of(&local)
            );
            assert_eq!(
                session_of(&local),
                ordinary,
                "{case}: {hour:02}:00 ET on the ordinary table is the venue's own session"
            );
            assert_eq!(
                half_day_session_of(&local),
                half_day,
                "{case}: and on the half-day table, whose only gap is the afternoon between its \
                 post session's close and the evening window's open"
            );
        }
        // The overnight grid's own buckets, named by the venue's series: its 45-minute
        // overnight candles are `20:00 / 20:45 / 21:30 / 22:15 / 23:00 / 23:45 / 00:30`, so
        // the last bucket it published before this run's window closed is 00:30 and the
        // minute the screen's 1m series ends on (00:39) belongs to it. `01:15` is the next
        // bucket on that same grid, which is why the assertion below is stated on an instant
        // that is *inside* one rather than on a boundary.
        let last_small_hour = market_instant(&today, 0, 39);
        assert_eq!(
            bucket_start(last_small_hour, Some(45 * 60_000)).map(market_clock),
            Some("00:30".to_owned()),
            "{case}: 00:39 ET rests on the overnight grid's 00:30 bucket — the bucket the venue's \
             own 45-minute series opens after 23:45 (§4.6, §14.1 item 3)"
        );
        assert_eq!(
            bucket_start(market_instant(&today, 1, 15), Some(45 * 60_000)).map(market_clock),
            Some("01:15".to_owned()),
            "{case}: and 01:15 is that grid's next bucket, not an interior point of the 00:30 one"
        );
        println!(
            "  §14.1 item 5: a non-regular print reaches a minute bucket ({}) and never a daily \
             one; the venue's own DAY candles are bounded by the regular session alone. The \
             venue publishes an *overnight* session too ({} candle(s) here), and it is one \
             window spanning the market's midnight — 20:00 to 04:00 ET, whose grid the venue's \
             own 45-minute series counts from 20:00 across the day boundary, as §14.1 item 3 \
             measured and §4.6 states",
            pre_post_minute_buckets.len(),
            non_regular.get("overnight").copied().unwrap_or(0)
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}
