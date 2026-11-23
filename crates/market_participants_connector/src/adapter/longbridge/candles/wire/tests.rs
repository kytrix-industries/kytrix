use std::time::Duration;

use futures::StreamExt as _;
use longbridge_proto::quote::{
    PushDepth, PushQuote, PushTrade, SecurityHistoryCandlestickRequest, SubscribeRequest, Trade,
    UnsubscribeRequest,
};
use market_participants_model::{decimal::UnixMillis, routing::CapabilityId};
use smol_str::SmolStr;

use super::{fetch_candlesticks, subscribe_candlesticks};
use crate::{
    adapter::{
        NativeCandlestick, ProtocolEvent, ProtocolResponse, SubscriptionHandle,
        longbridge::{
            candles::{
                aggregator::{CandleAggregator, CandlePrint, QuotePrint},
                session_table::{
                    HALF_DAY_SESSION_TABLE, MARKET_ZONE as ZONE, MarketSession,
                    NORMAL_SESSION_TABLE,
                },
            },
            codec,
            socket::{ConnectionPlan, establish},
            test_support::{
                AuthAnswer, CalendarAnswer, CandleAnswer, FixtureCandle, LongbridgeFixture,
                ProfileAnswer, QuoteAnswer, SUBSCRIPTION_REFUSAL_MESSAGE, TokenAnswer,
                credential_values, credentials, execution, resolved_listing,
            },
        },
    },
    catalog::resolved::ResolvedListingRef,
    error::MarketParticipantError,
    market::timeframe::{Adjustment, TimeFrame},
    market_participants::model::{CandlestickFetchQuery, CandlestickSubscriptionQuery},
};

fn at(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> UnixMillis {
    jiff::civil::date(year, month, day)
        .at(hour, minute, 0, 0)
        .to_zoned(jiff::tz::TimeZone::get(ZONE).expect("the market zone is in the database"))
        .expect("the fixture wall time is a real instant")
        .timestamp()
        .as_millisecond()
}

fn seconds(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> i64 {
    at(year, month, day, hour, minute) / 1_000
}

async fn listing() -> ResolvedListingRef {
    resolved_listing().await
}

fn fetch_query(
    listing: &ResolvedListingRef,
    start: Option<UnixMillis>,
    end: Option<UnixMillis>,
) -> CandlestickFetchQuery {
    CandlestickFetchQuery {
        listing: listing.key.clone(),
        timeframe: TimeFrame::Hour1,
        adjustment: Adjustment::None,
        start_timestamp: start,
        end_timestamp: end,
        limit: 300,
    }
}

fn subscription_query(listing: &ResolvedListingRef) -> CandlestickSubscriptionQuery {
    CandlestickSubscriptionQuery {
        listing: listing.key.clone(),
        timeframe: TimeFrame::Hour1,
        adjustment: Adjustment::Forward,
    }
}

async fn sent_history(
    fixture: &LongbridgeFixture,
    query: &CandlestickFetchQuery,
) -> SecurityHistoryCandlestickRequest {
    let context = execution(fixture, Some(credentials()), CapabilityId::Candlesticks);
    let plan = ConnectionPlan::of(&context, &credential_values())
        .expect("the fixture connection is planned");
    let session = establish(&context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection");
    let reference = listing().await;

    fetch_candlesticks(&context, &session, &reference, query)
        .await
        .expect("the fixture answers the history request");

    fixture.body_of::<SecurityHistoryCandlestickRequest>(
        crate::adapter::longbridge::codec::GET_SECURITY_HISTORY_CANDLESTICKS,
    )
}

fn offset_of(
    request: &SecurityHistoryCandlestickRequest,
) -> &longbridge_proto::quote::security_history_candlestick_request::OffsetQuery {
    request
        .offset_request
        .as_ref()
        .expect("every shape this build sends is an offset query")
}

#[tokio::test]
async fn the_start_alone_pages_forward_from_its_own_market_local_anchor() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let reference = listing().await;
    let sent = sent_history(
        &fixture,
        &fetch_query(&reference, Some(at(2026, 3, 6, 10, 5)), None),
    )
    .await;

    assert_eq!(sent.symbol, "AAPL.US");
    assert_eq!(sent.period, codec::LongbridgePeriod::Hour1.number());
    assert_eq!(sent.adjust_type, 0, "the request asked for no adjustment");
    assert_eq!(sent.query_type, codec::QUERY_BY_OFFSET);
    assert_eq!(sent.trade_session, codec::TRADE_SESSION_ALL);
    assert!(sent.date_request.is_none(), "a date query is never sent");

    let offset = offset_of(&sent);
    assert_eq!(offset.direction, codec::DIRECTION_FORWARD);
    assert_eq!(offset.date, "20260306");
    assert_eq!(
        offset.minute, "1005",
        "10:05 in New York, which is 15:05 UTC on that date"
    );
    assert_eq!(offset.count, 300, "count is the request's own limit");
}

#[tokio::test]
async fn the_end_alone_pages_backward_from_its_own_market_local_anchor() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let reference = listing().await;
    let sent = sent_history(
        &fixture,
        &fetch_query(&reference, None, Some(at(2026, 3, 9, 15, 45))),
    )
    .await;

    let offset = offset_of(&sent);
    assert_eq!(offset.direction, codec::DIRECTION_BACKWARD);
    assert_eq!(
        offset.date, "20260309",
        "the day after the March transition, in New York"
    );
    assert_eq!(
        offset.minute, "1545",
        "15:45 in New York, which is 19:45 UTC once the clocks have moved"
    );
    assert_eq!(offset.count, 300);
}

#[tokio::test]
async fn neither_anchor_pages_backward_with_no_anchor_at_all() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let reference = listing().await;
    let sent = sent_history(&fixture, &fetch_query(&reference, None, None)).await;

    let offset = offset_of(&sent);
    assert_eq!(offset.direction, codec::DIRECTION_BACKWARD);
    assert_eq!(offset.date, "");
    assert_eq!(offset.minute, "");
    assert_eq!(offset.count, 300);
}

#[tokio::test]
async fn both_anchors_page_backward_from_the_end() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let reference = listing().await;
    let sent = sent_history(
        &fixture,
        &fetch_query(
            &reference,
            Some(at(2026, 3, 6, 10, 5)),
            Some(at(2026, 3, 9, 15, 45)),
        ),
    )
    .await;

    let offset = offset_of(&sent);
    assert_eq!(
        offset.direction,
        codec::DIRECTION_BACKWARD,
        "the end is what the venue is anchored at"
    );
    assert_eq!(offset.date, "20260309");
    assert_eq!(offset.minute, "1545");
    assert_eq!(offset.count, 300);
}

#[test]
fn the_anchor_is_the_markets_own_wall_time() {
    let winter = at(2026, 1, 15, 9, 30);
    let summer = at(2026, 7, 15, 9, 30);

    assert_eq!(
        super::stream::market_anchor(winter),
        (SmolStr::new_static("20260115"), SmolStr::new_static("0930")),
        "09:30 in New York in January"
    );
    assert_eq!(
        super::stream::market_anchor(summer),
        (SmolStr::new_static("20260715"), SmolStr::new_static("0930")),
        "and 09:30 in New York in July, which is a different instant"
    );
    assert_ne!(
        winter / 1_000 % 86_400,
        summer / 1_000 % 86_400,
        "the two instants are an hour apart in UTC, which is the whole point"
    );
}

#[test]
fn seconds_become_milliseconds() {
    let candle = FixtureCandle::flat(15_000, 1_772_807_400);
    assert_eq!(
        super::stream::millis_of(candle.timestamp).expect("the fixture timestamp fits"),
        1_772_807_400_000
    );
    assert!(
        super::stream::millis_of(i64::MAX).is_err(),
        "a timestamp whose milliseconds do not fit is refused rather than wrapped"
    );
}

fn answer_frame(command: u8, request_id: u32, status: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(10 + body.len());
    frame.push(2);
    frame.push(command);
    frame.extend_from_slice(&request_id.to_be_bytes());
    frame.push(status);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    frame.extend_from_slice(body);
    frame
}

#[test]
fn an_answer_is_decoded_with_the_command_it_answers() {
    for command in [codec::SUBSCRIBE, codec::GET_TRADING_DAYS] {
        let payload = answer_frame(command, 7, 0, b"body");
        let crate::adapter::longbridge::wire::Frame::RequestAnswer {
            command_code,
            request_id,
            status,
            body,
        } = crate::adapter::longbridge::wire::decode(&payload)
            .expect("the fixture frame is the venue's own")
        else {
            panic!("command {command} answers a request this build reads");
        };
        assert_eq!(command_code, command);
        assert_eq!(request_id, 7);
        assert_eq!(status, 0);
        assert_eq!(body.as_ref(), b"body");
    }

    let unread = answer_frame(codec::QUERY_USER_QUOTE_PROFILE, 7, 0, b"");
    assert!(
        matches!(
            crate::adapter::longbridge::wire::decode(&unread)
                .expect("the fixture frame is the venue's own"),
            crate::adapter::longbridge::wire::Frame::RequestAnswer {
                command_code: codec::QUERY_USER_QUOTE_PROFILE,
                ..
            }
        ),
        "an answer nothing here reads is still carried as the answer it is"
    );
}

#[test]
fn the_subscription_topic_states_the_adjustment() {
    let symbol = SmolStr::new_static("AAPL.US");
    let period = codec::LongbridgePeriod::Hour1;

    assert_ne!(
        super::stream::topic_text(&symbol, period, Adjustment::None),
        super::stream::topic_text(&symbol, period, Adjustment::Forward),
        "the two adjustments are two topics"
    );
    assert_eq!(
        super::stream::topic_text(&symbol, period, Adjustment::None),
        SmolStr::new_static("AAPL.US:60:none")
    );
    assert_eq!(
        super::stream::topic_text(&symbol, period, Adjustment::Forward),
        SmolStr::new_static("AAPL.US:60:forward")
    );
    assert_ne!(
        super::stream::topic_text(&symbol, period, Adjustment::None),
        super::stream::topic_text(&symbol, codec::LongbridgePeriod::Hour4, Adjustment::None),
        "and so are two intervals"
    );
}

#[tokio::test]
async fn the_history_is_answered_in_ascending_order() {
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::of(vec![
            FixtureCandle::flat(12_000, seconds(2026, 6, 10, 12, 0)),
            FixtureCandle::flat(10_000, seconds(2026, 6, 10, 10, 0)),
            FixtureCandle::flat(11_000, seconds(2026, 6, 10, 11, 0)),
        ]),
        QuoteAnswer::default(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let plan = ConnectionPlan::of(&context, &credential_values())
        .expect("the fixture connection is planned");
    let session = establish(&context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection");
    let reference = listing().await;

    let response = fetch_candlesticks(
        &context,
        &session,
        &reference,
        &fetch_query(&reference, None, None),
    )
    .await
    .expect("the fixture answers the history request");

    let ProtocolResponse::Candlesticks(candles) = response else {
        panic!("a history fetch answers with candles");
    };
    assert_eq!(
        candles
            .iter()
            .map(|candle| candle.data.timestamp)
            .collect::<Vec<_>>(),
        vec![
            at(2026, 6, 10, 10, 0),
            at(2026, 6, 10, 11, 0),
            at(2026, 6, 10, 12, 0)
        ],
        "the page is ascending, and every timestamp is the venue's seconds in milliseconds"
    );
}

#[tokio::test]
async fn the_two_anchored_shape_drops_what_the_start_excludes() {
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::of(vec![
            FixtureCandle::flat(10_000, seconds(2026, 6, 10, 10, 0)),
            FixtureCandle::flat(11_000, seconds(2026, 6, 10, 11, 0)),
            FixtureCandle::flat(12_000, seconds(2026, 6, 10, 12, 0)),
        ]),
        QuoteAnswer::default(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let plan = ConnectionPlan::of(&context, &credential_values())
        .expect("the fixture connection is planned");
    let session = establish(&context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection");
    let reference = listing().await;

    let response = fetch_candlesticks(
        &context,
        &session,
        &reference,
        &fetch_query(
            &reference,
            Some(at(2026, 6, 10, 11, 0)),
            Some(at(2026, 6, 10, 12, 0)),
        ),
    )
    .await
    .expect("the fixture answers the history request");

    let ProtocolResponse::Candlesticks(candles) = response else {
        panic!("a history fetch answers with candles");
    };
    assert_eq!(
        candles
            .iter()
            .map(|candle| candle.data.timestamp)
            .collect::<Vec<_>>(),
        vec![at(2026, 6, 10, 11, 0), at(2026, 6, 10, 12, 0)],
        "the candle before the start was dropped, and the start's own was kept"
    );
}

#[tokio::test]
async fn the_subscription_asks_for_its_screen_with_the_requests_adjustment() {
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::of(vec![FixtureCandle::flat(
            10_000,
            seconds(2026, 6, 10, 10, 0),
        )]),
        QuoteAnswer::default(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let plan = ConnectionPlan::of(&context, &credential_values())
        .expect("the fixture connection is planned");
    let session = establish(&context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection");
    let reference = listing().await;

    let handle = subscribe_candlesticks(
        &context,
        &session,
        &reference,
        &subscription_query(&reference),
    )
    .await
    .expect("the fixture answers the screen and accepts the subscription");
    drop(handle);

    fixture.await_command(codec::SUBSCRIBE).await;
    let screen: longbridge_proto::quote::SecurityCandlestickRequest =
        fixture.body_of(codec::GET_SECURITY_CANDLESTICKS);
    assert_eq!(screen.symbol, "AAPL.US");
    assert_eq!(screen.count, codec::FIRST_SCREEN_COUNT);
    assert_eq!(screen.trade_session, codec::TRADE_SESSION_ALL);
    assert_eq!(
        screen.adjust_type,
        codec::LongbridgeAdjust::ForwardAdjust.number(),
        "the screen follows the request's own adjustment, which is the deliberate \
             difference from the official client"
    );

    let subscribe: SubscribeRequest = fixture.body_of(codec::SUBSCRIBE);
    assert_eq!(subscribe.symbol, vec!["AAPL.US".to_owned()]);
    assert_eq!(
        subscribe.sub_type,
        vec![codec::SUB_TYPE_QUOTE, codec::SUB_TYPE_TRADE]
    );
    assert!(
        subscribe.is_first_push,
        "the venue pushes a snapshot at once"
    );

    let unsubscribe: UnsubscribeRequest = fixture.body_of(codec::UNSUBSCRIBE);
    assert_eq!(unsubscribe.symbol, vec!["AAPL.US".to_owned()]);
    assert_eq!(
        unsubscribe.sub_type,
        vec![codec::SUB_TYPE_QUOTE, codec::SUB_TYPE_TRADE]
    );
    assert!(!unsubscribe.unsub_all, "only this symbol is unsubscribed");
}

#[tokio::test]
async fn the_screen_is_answered_before_the_pushes_and_the_open_candle_rolls() {
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::of(vec![
            FixtureCandle::flat(9_000, seconds(2026, 6, 10, 9, 0)),
            FixtureCandle::flat(10_000, seconds(2026, 6, 10, 10, 0)),
        ]),
        QuoteAnswer::default(),
    )
    .await;
    fixture.push_quote(&PushQuote {
        symbol: "AAPL.US".to_owned(),
        last_done: "101.00".to_owned(),
        open: "100.00".to_owned(),
        high: "101.00".to_owned(),
        low: "100.00".to_owned(),
        timestamp: seconds(2026, 6, 10, 10, 30),
        volume: 11,
        current_volume: 1,
        current_turnover: "101.00".to_owned(),
        trade_status: 0,
        trade_session: 0,
        sequence: 1,
        turnover: String::new(),
        tag: 0,
    });
    fixture.push_depth(&PushDepth {
        symbol: "AAPL.US".to_owned(),
        sequence: 3,
        ask: Vec::new(),
        bid: Vec::new(),
    });
    fixture.push_trades(&PushTrade {
        symbol: "AAPL.US".to_owned(),
        sequence: 2,
        trade: vec![Trade {
            price: "102.00".to_owned(),
            volume: 2,
            timestamp: seconds(2026, 6, 10, 11, 0),
            trade_type: String::new(),
            direction: 0,
            trade_session: 0,
        }],
    });

    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let plan = ConnectionPlan::of(&context, &credential_values())
        .expect("the fixture connection is planned");
    let session = establish(&context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection");
    let reference = listing().await;
    let handle = subscribe_candlesticks(
        &context,
        &session,
        &reference,
        &subscription_query(&reference),
    )
    .await
    .expect("the fixture answers the screen and accepts the subscription");

    let events = drain(handle, 5, &fixture).await;
    let candles: Vec<_> = events
        .iter()
        .map(|event| match event {
            ProtocolEvent::Candlestick(candle) => candle.data.clone(),
            other => panic!(
                "a candle stream answers with candles, got {}",
                event_name(other)
            ),
        })
        .collect();

    assert_eq!(
        candles[0].timestamp,
        at(2026, 6, 10, 9, 0),
        "the screen is answered first, in the order the venue sent it"
    );
    assert_eq!(candles[1].timestamp, at(2026, 6, 10, 10, 0));
    assert_eq!(
        candles[2].timestamp,
        at(2026, 6, 10, 10, 0),
        "then the quote of 10:30 updates the bucket that opened at 10:00, because the \
         quote's own grid is still the one the screen left off at"
    );
    assert_eq!(
        candles[2].close, candles[1].close,
        "and the update states the candle as it stood: the quote arrived after this \
         bucket's grid"
    );
    assert_eq!(
        candles[3].timestamp,
        at(2026, 6, 10, 10, 30),
        "the trade of 11:00 is inside the session's second hourly bucket, which opened at \
         10:30 — the regular session's grid starts at its own open, not at a UTC hour. \
         The depth frame between the two pushes produced no event at all"
    );
    assert_eq!(
        candles[4].timestamp,
        at(2026, 6, 10, 10, 30),
        "and the trade of 11:00 updates that same candle rather than opening one: the \
         session's grid is 09:30, 10:30, 11:30, so 11:00 is still inside the bucket that \
         opened at 10:30"
    );
    assert_eq!(
        candles[4].close.to_string(),
        "102",
        "which the trade's own price now closes"
    );
    assert_eq!(
        candles[4].open, candles[3].open,
        "and whose open is still where the roll left it"
    );
    assert_eq!(
        candles[4].volume,
        market_participants_model::decimal::Quantity::from(3_u8),
        "with the volumes of the quote and the trade added up"
    );
}

#[tokio::test]
async fn a_refused_subscription_ends_the_stream_with_the_venues_own_words() {
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::of(vec![FixtureCandle::flat(
            10_000,
            seconds(2026, 6, 10, 10, 0),
        )]),
        QuoteAnswer::default(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let plan = ConnectionPlan::of(&context, &credential_values())
        .expect("the fixture connection is planned");
    let session = establish(&context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection");
    let reference = listing().await;
    let handle = subscribe_candlesticks(
        &context,
        &session,
        &reference,
        &subscription_query(&reference),
    )
    .await
    .expect("the screen is granted even though the subscription is refused");

    fixture.serve_candles(CandleAnswer {
        status: 0,
        candles: vec![FixtureCandle::flat(10_000, seconds(2026, 6, 10, 10, 0))],
        subscribe_status: 1,
    });

    let mut stream = handle.into_stream();
    let mut answer = None;
    for _ in 0..4 {
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        match next {
            Ok(Some(Ok(ProtocolEvent::Candlestick(_)))) => continue,
            Ok(Some(Ok(other))) => panic!(
                "a candle stream answers with candles: {}",
                event_name(&other)
            ),
            Ok(Some(Err(error))) => {
                answer = Some(error);
                break;
            }
            Ok(None) => break,
            Err(_) => panic!("the refusal arrives within the bound"),
        }
    }

    let error = answer.expect("a refused subscription ends the stream with why");
    assert!(
        matches!(
            &error,
            MarketParticipantError::Rejected { code, message, .. }
                if code == "3000000" && message == SUBSCRIPTION_REFUSAL_MESSAGE
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_half_day_from_the_venue_moves_the_afternoon_grid() {
    let screen_at = seconds(2026, 6, 11, 9, 30);
    let quote_at = seconds(2026, 6, 11, 13, 35);
    let trade_at = seconds(2026, 6, 11, 13, 45);
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::of(vec![FixtureCandle::flat(100, screen_at)]),
        QuoteAnswer::default(),
    )
    .await;
    fixture.serve_half_days(vec![market_day(trade_at * 1_000)]);
    fixture.push_quote(&PushQuote {
        symbol: "AAPL.US".to_owned(),
        last_done: "101.00".to_owned(),
        open: "100.00".to_owned(),
        high: "101.00".to_owned(),
        low: "100.00".to_owned(),
        timestamp: quote_at,
        volume: 11,
        current_volume: 1,
        current_turnover: "101.00".to_owned(),
        trade_status: 0,
        trade_session: 0,
        sequence: 1,
        turnover: String::new(),
        tag: 0,
    });
    fixture.push_trades(&PushTrade {
        symbol: "AAPL.US".to_owned(),
        sequence: 2,
        trade: vec![Trade {
            price: "102.00".to_owned(),
            volume: 2,
            timestamp: trade_at,
            trade_type: String::new(),
            direction: 0,
            trade_session: 0,
        }],
    });

    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let plan = ConnectionPlan::of(&context, &credential_values())
        .expect("the fixture connection is planned");
    let session = establish(&context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection");
    let reference = listing().await;
    let handle = subscribe_candlesticks(
        &context,
        &session,
        &reference,
        &subscription_query(&reference),
    )
    .await
    .expect("the fixture answers the screen and accepts the subscription");

    let events = drain(handle, 4, &fixture).await;
    let ProtocolEvent::Candlestick(screen) = &events[0] else {
        panic!("the first event is the screen's own candle");
    };
    assert_eq!(
        screen.data.timestamp,
        screen_at * 1_000,
        "the screen is answered before the push, exactly as the venue sent it"
    );
    let ProtocolEvent::Candlestick(confirmed) = &events[1] else {
        panic!("the second event is the candle the screen left off at");
    };
    assert_eq!(
        confirmed.data.timestamp, screen.data.timestamp,
        "which the quote of 13:35 confirms, because the half day's grid puts that instant \
         in a bucket of its own"
    );
    assert_eq!(confirmed.data.close, screen.data.close);
    let ProtocolEvent::Candlestick(opened) = &events[2] else {
        panic!("the third event is the candle the quote opened");
    };
    assert_eq!(
        market_clock(opened.data.timestamp),
        (13, 0),
        "13:35 is inside the candle that opened at 13:00 local: the half day's post \
         session opens where its regular one closes, so its hourly grid is counted from \
         there. An ordinary grid would answer a candle that opened at 13:30, which is the \
         difference this assertion exists to read"
    );
    assert_eq!(
        opened.data.open,
        quote_price(101),
        "the quote that opened the candle is its open, high, low and close: the venue's own \
         rule writes every price field from the print that opened a bucket, and the screen's \
         closing price is the continuity value only for a bucket no print has priced yet"
    );
    assert_eq!(opened.data.close.to_string(), "101");
    let ProtocolEvent::Candlestick(updated) = &events[3] else {
        panic!("the fourth event is the candle the trade updated");
    };
    assert_eq!(
        updated.data.timestamp, opened.data.timestamp,
        "and the trade of 13:45 is inside that same candle rather than one of its own"
    );
    assert_eq!(
        updated.data.volume,
        market_participants_model::decimal::Quantity::from(3_u8),
        "which holds the increment of both prints"
    );
    assert_eq!(updated.data.close.to_string(), "102");

    let request: longbridge_proto::quote::MarketTradeDayRequest =
        fixture.body_of(codec::GET_TRADING_DAYS);
    assert_eq!(request.market, "US");
    assert_eq!(
        request.beg_day, "20260606",
        "five days before the push's day"
    );
    assert_eq!(request.end_day, "20260711", "and thirty days after it");
}

#[test]
fn the_two_tables_rest_the_afternoon_on_different_instants() {
    let quote = CandlePrint::quote(QuotePrint {
        timestamp: at(2026, 6, 11, 13, 35),
        session: MarketSession::Normal,
        price: quote_price(101),
        volume_delta: None,
        cumulative: None,
    });
    let screen = CandlePrint::quote(QuotePrint {
        timestamp: at(2026, 6, 11, 9, 30),
        session: MarketSession::Normal,
        price: quote_price(100),
        volume_delta: None,
        cumulative: None,
    });

    assert_eq!(
        bucket_open(&screen, &HALF_DAY_SESSION_TABLE),
        bucket_open(&screen, &NORMAL_SESSION_TABLE),
        "both tables agree that the screen's own instant is the market's open"
    );
    assert_eq!(
        bucket_open(&quote, &HALF_DAY_SESSION_TABLE),
        Some(at(2026, 6, 11, 13, 0)),
        "a half day's afternoon hourly grid is counted from the post open at 13:00"
    );
    assert_eq!(
        bucket_open(&quote, &NORMAL_SESSION_TABLE),
        Some(at(2026, 6, 11, 13, 30)),
        "while an ordinary day's is counted from the post open at 16:00, so the same \
         instant belongs to the candle that opened at 13:30 — the difference the wire \
         case reads off the push"
    );
}

fn bucket_open(
    print: &CandlePrint,
    table: &crate::adapter::longbridge::candles::session_table::SessionTable,
) -> Option<UnixMillis> {
    CandleAggregator::from_first_screen(
        TimeFrame::Hour1,
        clock_zone(),
        NativeCandlestick {
            timestamp: at(2026, 6, 11, 9, 30),
            open: quote_price(100),
            high: quote_price(100),
            low: quote_price(100),
            close: quote_price(100),
            volume: 1_u8.into(),
        },
    )
    .bucket_of(print, table)
    .and_then(|bucket| bucket.start(&clock_zone()))
    .map(|opened| opened.timestamp().as_millisecond())
}

fn quote_price(cents: u64) -> market_participants_model::decimal::Price {
    market_participants_model::decimal::Price::from(cents)
}

fn clock_zone() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(ZONE).expect("the market zone is in the database")
}

#[tokio::test]
async fn a_refused_calendar_is_asked_about_once() {
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::of(vec![FixtureCandle::flat(
            10_000,
            seconds(2026, 6, 11, 9, 30),
        )]),
        QuoteAnswer::default(),
    )
    .await;
    fixture.serve_calendar(CalendarAnswer {
        status: 1,
        half_days: Vec::new(),
    });
    for minute in [35, 40] {
        fixture.push_trades(&PushTrade {
            symbol: "AAPL.US".to_owned(),
            sequence: 2,
            trade: vec![Trade {
                price: "101.00".to_owned(),
                volume: 1,
                timestamp: seconds(2026, 6, 11, 9, minute),
                trade_type: String::new(),
                direction: 0,
                trade_session: 0,
            }],
        });
    }

    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let plan = ConnectionPlan::of(&context, &credential_values())
        .expect("the fixture connection is planned");
    let session = establish(&context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection");
    let reference = listing().await;
    let handle = subscribe_candlesticks(
        &context,
        &session,
        &reference,
        &subscription_query(&reference),
    )
    .await
    .expect("the fixture answers the screen and accepts the subscription");

    let events = drain(handle, 3, &fixture).await;
    for (index, event) in events.iter().enumerate() {
        let ProtocolEvent::Candlestick(candle) = event else {
            panic!("event {index} is a candle of the ordinary day's grid");
        };
        assert_eq!(
            market_clock(candle.data.timestamp),
            (9, 30),
            "every candle is still cut on the ordinary grid: the calendar failed open"
        );
    }
    assert_eq!(
        fixture
            .commands()
            .iter()
            .filter(|command| **command == codec::GET_TRADING_DAYS)
            .count(),
        1,
        "the refused calendar was asked about once for the whole stream, not once per push"
    );
}

fn market_clock(millis: UnixMillis) -> (i8, i8) {
    let local = jiff::Zoned::new(
        jiff::Timestamp::from_millisecond(millis).expect("the fixture instant is real"),
        jiff::tz::TimeZone::get(ZONE).expect("the market zone is in the database"),
    )
    .datetime();
    (local.hour(), local.minute())
}

fn market_day(millis: UnixMillis) -> String {
    let local = jiff::Zoned::new(
        jiff::Timestamp::from_millisecond(millis).expect("the fixture instant is real"),
        jiff::tz::TimeZone::get(ZONE).expect("the market zone is in the database"),
    )
    .date();
    format!("{}{:02}{:02}", local.year(), local.month(), local.day())
}

fn event_name(event: &ProtocolEvent) -> &'static str {
    match event {
        ProtocolEvent::Candlestick(_) => "a candle",
        ProtocolEvent::Ticker(_) => "a ticker",
        ProtocolEvent::OrderBook(_) => "an order book",
        ProtocolEvent::Trade(_) => "a trade",
        ProtocolEvent::Order(_) => "an order",
        ProtocolEvent::Position(_) => "a position",
        ProtocolEvent::ForceOrder(_) => "a liquidation",
        ProtocolEvent::HyperliquidWalletUpdate(_) => "a wallet update",
        ProtocolEvent::PredictionSeries(_) => "prediction series updates",
        ProtocolEvent::PredictionOrderBookUpdate(_) => "prediction book updates",
    }
}

async fn drain(
    handle: SubscriptionHandle,
    count: usize,
    fixture: &LongbridgeFixture,
) -> Vec<ProtocolEvent> {
    let mut stream = handle.into_stream();
    let mut events = Vec::with_capacity(count);
    for _ in 0..count {
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        match next {
            Ok(Some(Ok(event))) => events.push(event),
            Ok(Some(Err(error))) => panic!(
                "the fixture answers with events, got {error} after {} events; \
                     the venue received {:?}",
                events.len(),
                fixture.commands()
            ),
            Ok(None) => panic!(
                "the stream ends after {} events; the venue received {:?}",
                events.len(),
                fixture.commands()
            ),
            Err(_) => panic!(
                "the fixture answers within the bound; it answered {} events and the \
                     venue received {:?}",
                events.len(),
                fixture.commands()
            ),
        }
    }
    events
}
