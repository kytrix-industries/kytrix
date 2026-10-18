//! The unit tests of the candle wire.
//!
//! They are a file of their own rather than a block inside the module because the module
//! boundary is what they belong to, not the file: what they assert is what the wire sends
//! and what it makes of a frame, and they reach every private item of it through `super::`.

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

/// One market-local wall time as milliseconds since the epoch.
fn at(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> UnixMillis {
    jiff::civil::date(year, month, day)
        .at(hour, minute, 0, 0)
        .to_zoned(jiff::tz::TimeZone::get(ZONE).expect("the market zone is in the database"))
        .expect("the fixture wall time is a real instant")
        .timestamp()
        .as_millisecond()
}

/// One market-local wall time as the venue's Unix seconds.
fn seconds(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> i64 {
    at(year, month, day, hour, minute) / 1_000
}

/// The one listing every case here is about, as the gateway would resolve it.
///
/// The reference is produced by publishing the venue's own adapter entry through a
/// catalogue and resolving it back, which is where the adapter's references come from:
/// the wire reads the venue's symbol and the typed identifiers off the resolution, and
/// a reference assembled by hand could say anything.
async fn listing() -> ResolvedListingRef {
    resolved_listing().await
}

/// One fetch query with the anchors the case states.
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

/// One subscription query of the fixture listing.
fn subscription_query(listing: &ResolvedListingRef) -> CandlestickSubscriptionQuery {
    CandlestickSubscriptionQuery {
        listing: listing.key.clone(),
        timeframe: TimeFrame::Hour1,
        adjustment: Adjustment::Forward,
    }
}

/// Runs one fetch over the fixture and answers what the venue received.
async fn sent_history(
    fixture: &LongbridgeFixture,
    query: &CandlestickFetchQuery,
) -> SecurityHistoryCandlestickRequest {
    let context = execution(fixture, credentials(), CapabilityId::Candlesticks);
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

/// The offset request one fetch sent, which every shape below is read through.
fn offset_of(
    request: &SecurityHistoryCandlestickRequest,
) -> &longbridge_proto::quote::security_history_candlestick_request::OffsetQuery {
    request
        .offset_request
        .as_ref()
        .expect("every shape this build sends is an offset query")
}

/// 1. A `start` alone pages **forward** from it, anchored at the start's own
/// market-local time.
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

/// 2. An `end` alone pages **backward** from it, and the anchor is the market's own
/// wall time on the day after the March transition — which is what makes this about
/// the market's clock rather than about UTC.
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

/// 3. Neither anchor pages **backward** from the venue's newest candle, with both
/// anchor fields left empty: an empty `date`/`minute` is what "no anchor" means on
/// this wire.
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

/// 4. Both anchors page **backward** from `end`, and the caller's `start` is applied
/// by the adapter: the venue has no way to say "backwards from here, but not before
/// there".
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

/// The same instant formats differently on either side of a daylight-saving change,
/// because the anchor is the market's wall time rather than a fixed offset.
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

/// The venue's timestamps are Unix seconds and the model's are milliseconds.
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

/// One response frame of the protocol, as the venue sends it.
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

/// Every answer this build reads is decoded into the command it answers rather than
/// into one command's answer: a subscribe frame's answer is what a stream recognises,
/// and a trading-day frame's answer is what the calendar reader recognises, and a
/// decoder that answered only one of them would leave the other reading a push that
/// never arrived.
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

/// The topic one subscription is shared under states every term its frame states, and
/// the adjustment is one of them: the venue's subscribe frame carries no adjustment, so
/// two readers of one symbol and interval under different conventions are two frames,
/// and a topic that merged them would leave the second reader waiting for an answer that
/// names the first reader's id.
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

/// The history is answered in ascending order however the venue sent it, because the
/// widget reads a page shorter than the request as "the newest edge is reached".
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
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);
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

/// The "both anchors" shape drops the candles the caller excluded: the venue pages
/// backwards from `end` and has no way to say "but not before here".
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
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);
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

/// The subscription asks for its opening screen before it registers, with the
/// request's own adjustment rather than the official client's fixed one.
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
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);
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

/// The opening screen is answered first and in full, and the pushes that follow it are
/// read in the order the venue sent them: a quote of the candle the screen left off at,
/// a book this build does not read, and a trade of the next bucket.
///
/// What one print does to the candle it belongs to is the aggregation's own case; what
/// this one states is the stream's: the screen precedes every push, a frame no
/// subscription reads produces no event, and a roll is two events rather than one.
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
    // The venue's own pushes, queued before the subscription so they follow its frames.
    // The fixture delivers one per frame the subscription sends after it subscribes, in
    // the order it was given them, which is the order the venue sends pushes in.
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

    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);
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

/// A subscription the venue refuses ends the stream with the venue's own words: the
/// subscribe answer is not a pending request, so this is the one place it is read.
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
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);
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

    // The venue refuses the subscription while granting this screen, which is the case
    // only the stream can read: the subscribe frame is not a pending request.
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

/// The half-day table reaches the aggregation over the wire: the venue's own cmd 9
/// answer is decoded, its half day is installed, and a push on that day is bucketed by
/// the half day's own grid rather than the ordinary one.
///
/// The assertion is discriminating because the two grids rest the afternoon on different
/// instants: a half day's post session opens at 13:00 where an ordinary day's opens at
/// 16:00, so the candle 13:35 belongs to opens at 13:00 on one table and at 13:30 on the
/// other. The screen is the market's own open, which both tables agree about, so the
/// candle whose boundary is asserted is one only the calendar can explain.
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
    // The first push is what the calendar is read before, and it carries the half day's
    // own date; the second is the print the two grids disagree about.
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

    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);
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

/// The two tables the case above reads apart, stated as instants: a half day's post
/// session opens where its regular one closes, so 13:35 belongs to a candle that opened
/// at 13:00 on one table and to one that opened at 13:30 on the other. The screen's own
/// instant is the market's open, which both tables agree about — which is why the wire
/// case can start from it and still tell the two grids apart.
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

/// The instant one print's bucket opens at, under one table.
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

/// One price of one whole cent, in the model's own scaled units.
fn quote_price(cents: u64) -> market_participants_model::decimal::Price {
    market_participants_model::decimal::Price::from(cents)
}

/// The market's own zone.
fn clock_zone() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(ZONE).expect("the market zone is in the database")
}

/// A calendar the venue will not state is asked about once, not once per push: the
/// stream records the read — and what it found, which is nothing — and the ordinary
/// table stays in force (§4.6's fail-open).
///
/// The count of cmd 9 frames is the assertion, because the failure mode is a request
/// repeated for every frame the venue ever sends.
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
    // Two pushes of the same hourly bucket, so the stream merges twice and each merge is
    // a chance to ask the venue again.
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

    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);
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

/// The market-local hour and minute of one instant, as the assertion above reads it.
///
/// A boundary is a statement about the market's own clock, so the test asserts the
/// clock rather than an epoch millisecond: the two are the same fact, and the clock is
/// the one a reader of this test can check against the venue's calendar.
fn market_clock(millis: UnixMillis) -> (i8, i8) {
    let local = jiff::Zoned::new(
        jiff::Timestamp::from_millisecond(millis).expect("the fixture instant is real"),
        jiff::tz::TimeZone::get(ZONE).expect("the market zone is in the database"),
    )
    .datetime();
    (local.hour(), local.minute())
}

/// The market-local day one instant fell in, as the venue's own `YYYYMMDD`.
fn market_day(millis: UnixMillis) -> String {
    let local = jiff::Zoned::new(
        jiff::Timestamp::from_millisecond(millis).expect("the fixture instant is real"),
        jiff::tz::TimeZone::get(ZONE).expect("the market zone is in the database"),
    )
    .date();
    format!("{}{:02}{:02}", local.year(), local.month(), local.day())
}

/// The name of one event, for a failure message that does not print a whole value.
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
    }
}

/// Reads the first `count` events of one subscription, failing rather than hanging.
///
/// The events read so far travel with a failure: a stream that answered two of three
/// is a different diagnosis from one that answered none, and a bare "the fixture did
/// not answer" would hide which.
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
