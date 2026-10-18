//! The unit tests of the ticker wire.
//!
//! They are a file of their own rather than a block inside the module because the module
//! boundary is what they belong to, not the file: what they assert is what the wire sends
//! and what it makes of a frame, and they reach every private item of it through `super::`.
//!
//! Two properties are what the whole file is about. The first is that a subscription's
//! first event is the snapshot the wire itself pulled rather than a push the venue may or
//! may not send. The second is that the venue's pushes state only the fields that changed,
//! so a snapshot is what the merge of a push into the baseline makes it — and a case that
//! only ever sent one push could not tell that merge from no merge at all.

use std::time::Duration;

use futures::StreamExt as _;
use longbridge_proto::quote::{
    PushDepth, PushQuote, SecurityQuote, SubscribeRequest, UnsubscribeRequest,
};
use market_participants_model::{
    decimal::{Price, Quantity},
    routing::CapabilityId,
};
use smol_str::SmolStr;

use super::{fetch_ticker, subscribe_ticker};
use crate::{
    adapter::{
        NativeTicker, ProtocolEvent, ProtocolResponse, SubscriptionHandle,
        longbridge::{
            codec,
            socket::{ConnectionPlan, establish},
            symbols::market_symbol,
            test_support::{
                AuthAnswer, CandleAnswer, LongbridgeFixture, ProfileAnswer, QuoteAnswer,
                SUBSCRIPTION_REFUSAL_MESSAGE, TokenAnswer, credential_values, credentials,
                execution, resolved_listing,
            },
        },
    },
    catalog::resolved::ResolvedListingRef,
    error::{AuthError, MarketParticipantError},
};

/// The one listing these fixtures are about, as the published catalogue resolves it.
async fn listing() -> ResolvedListingRef {
    resolved_listing().await
}

/// The symbol the catalogue published for the fixture listing.
///
/// The wire reads it through the same function production reads it through, so a case
/// that compared against a symbol spelled here would be comparing against itself.
fn symbol_of(listing: &ResolvedListingRef) -> SmolStr {
    market_symbol(listing).expect("the fixture listing is a US equity of this venue")
}

/// One wire price, read the way the wire reads it.
fn price(value: &str) -> Price {
    value.parse().expect("the fixture price is a decimal")
}

/// A session on the fixture venue, established the way the adapter establishes one.
async fn session(
    context: &crate::adapter::ExecutionContext,
) -> std::sync::Arc<crate::adapter::longbridge::auth::LongbridgeSession> {
    let plan = ConnectionPlan::of(context, &credential_values())
        .expect("the fixture connection is planned");
    establish(context, &credential_values(), &plan)
        .await
        .expect("the fixture venue authenticates the connection")
}

/// One complete quote of one symbol, as the venue publishes it.
fn quote(symbol: &str) -> SecurityQuote {
    SecurityQuote {
        symbol: symbol.to_owned(),
        last_done: "105.00".to_owned(),
        prev_close: "99.00".to_owned(),
        open: "100.00".to_owned(),
        high: "110.00".to_owned(),
        low: "90.00".to_owned(),
        timestamp: 1_781_097_600,
        volume: 1_000,
        turnover: "105000.00".to_owned(),
        trade_status: 0,
        pre_market_quote: None,
        post_market_quote: None,
        over_night_quote: None,
    }
}

/// One push of one symbol that states what the case wants it to state.
///
/// Every field is settable because that is the property under test: a push states the
/// fields that changed and leaves the rest at their defaults.
fn push(symbol: &str, sequence: i64) -> PushQuote {
    PushQuote {
        symbol: symbol.to_owned(),
        last_done: String::new(),
        open: String::new(),
        high: String::new(),
        low: String::new(),
        timestamp: 1_781_097_600,
        volume: 0,
        turnover: String::new(),
        trade_status: 0,
        trade_session: 0,
        current_volume: 0,
        current_turnover: String::new(),
        sequence,
        tag: 0,
    }
}

/// One fixture whose quote command answers one complete quote of the fixture symbol.
async fn fixture_serving(quotes: Vec<SecurityQuote>) -> LongbridgeFixture {
    LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::default(),
        QuoteAnswer { status: 0, quotes },
    )
    .await
}

/// The ticker one event carries.
fn ticker_of(event: &ProtocolEvent) -> &NativeTicker {
    match event {
        ProtocolEvent::Ticker(ticker) => &ticker.data,
        other => panic!(
            "a ticker stream answers with tickers, got {}",
            event_name(other)
        ),
    }
}

/// What one event is, for a failure that has to say what arrived instead.
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
                "the fixture answers with events, got {error} after {} events; the venue \
                 received {:?}",
                events.len(),
                fixture.commands()
            ),
            Ok(None) => panic!(
                "the stream ends after {} events; the venue received {:?}",
                events.len(),
                fixture.commands()
            ),
            Err(_) => panic!(
                "the fixture answers within the bound; it answered {} events and the venue \
                 received {:?}",
                events.len(),
                fixture.commands()
            ),
        }
    }
    events
}

/// One quote pull answers every field of the venue's own quote, and it publishes no
/// ordering marker because the pull command carries none.
///
/// The traded notional is read as the amount it is: its unit — the listing's quote asset
/// — is attached one layer up, by the capability boundary that knows the listing's rules,
/// so what this asserts is the number the venue stated.
#[tokio::test]
async fn a_pull_answers_the_venues_whole_quote() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(vec![quote(&symbol)]).await;
    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;

    let response = fetch_ticker(&context, &session, &reference)
        .await
        .expect("the fixture answers the real-time quote");

    let ProtocolResponse::Ticker(answered) = response else {
        panic!("a ticker fetch answers with a ticker");
    };
    assert_eq!(answered.listing, reference.key);
    assert_eq!(
        answered.data.close,
        price("105.00"),
        "the last price is the venue's `last_done`"
    );
    assert_eq!(answered.data.open, price("100.00"));
    assert_eq!(answered.data.high, Some(price("110.00")));
    assert_eq!(answered.data.low, Some(price("90.00")));
    assert_eq!(
        answered.data.traded_quantity,
        Some(Quantity::from(1_000_u64)),
        "the traded quantity is the venue's `volume`"
    );
    assert_eq!(
        answered.data.traded_notional,
        Some(Quantity::from(105_000_u64)),
        "and the traded notional is its `turnover`, as the amount it is rather than as a price"
    );
    assert_eq!(
        answered.data.monotonic_id, None,
        "a pull publishes no sequence, so the snapshot says so rather than inventing one"
    );
}

/// A pull the venue answered with no quote is a protocol failure rather than an empty
/// answer: one request asked about one symbol, and a row built from nothing would show a
/// price that belongs to no listing.
#[tokio::test]
async fn a_pull_without_a_quote_is_a_protocol_failure() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(Vec::new()).await;
    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;

    let error = fetch_ticker(&context, &session, &reference)
        .await
        .expect_err("an answer with no quote is not a quote");

    assert!(
        matches!(
            &error,
            MarketParticipantError::ProtocolError { operation, reason, .. }
                if operation == "decode Longbridge quote" && reason.contains(symbol.as_str())
        ),
        "{error}"
    );
}

/// A pull the venue answered about another symbol is a protocol failure: the answer is
/// not this listing's, and drawing it would put one instrument's price on another's row.
#[tokio::test]
async fn a_pull_about_another_symbol_is_a_protocol_failure() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(vec![quote("MSFT.US")]).await;
    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;

    let error = fetch_ticker(&context, &session, &reference)
        .await
        .expect_err("an answer about another symbol is not this listing's quote");

    assert!(
        matches!(
            &error,
            MarketParticipantError::ProtocolError { reason, .. }
                if reason.contains("MSFT.US") && reason.contains(symbol.as_str())
        ),
        "{error}"
    );
}

/// An account whose profile grants no US market data is refused before any quote frame
/// exists, which is what the pull command's own place in the market-data set buys it.
#[tokio::test]
async fn a_pull_without_permission_is_refused_before_the_frame() {
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::without_us_market_data(),
        CandleAnswer::default(),
        QuoteAnswer {
            status: 0,
            quotes: vec![quote("AAPL.US")],
        },
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;
    let reference = listing().await;

    let error = fetch_ticker(&context, &session, &reference)
        .await
        .expect_err("the fixture account may not read US market data");

    assert!(
        matches!(
            error,
            MarketParticipantError::AuthenticationFailed {
                source: AuthError::NoMarketDataPermission,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(
        fixture
            .settled_frames()
            .await
            .iter()
            .map(|frame| frame.command)
            .collect::<Vec<_>>(),
        vec![codec::COMMAND_CODE_AUTH, codec::QUERY_USER_QUOTE_PROFILE],
        "the gate is what refused it, and the quote frame was never sent"
    );
}

/// A subscription registers the symbol the catalogue published, asks for quotes alone and
/// asks for no opening push, and it reads its baseline before the subscribe frame exists.
///
/// The order is the design's: a baseline the venue has not answered yet is not a baseline,
/// so the pull is what the subscribe frame waits for.
#[tokio::test]
async fn a_subscription_pulls_its_baseline_and_then_registers() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(vec![quote(&symbol)]).await;
    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;

    let handle = subscribe_ticker(&context, &session, &reference)
        .await
        .expect("the fixture answers the baseline and accepts the subscription");
    // Dropping the handle is what releases the lease, which is when the venue is told to
    // stop pushing; both directions are read back afterwards.
    drop(handle);

    fixture.await_command(codec::SUBSCRIBE).await;
    let sent: SubscribeRequest = fixture.body_of(codec::SUBSCRIBE);
    assert_eq!(
        sent.symbol,
        vec![symbol.as_str().to_owned()],
        "the symbol is the one the catalogue published"
    );
    assert_eq!(
        sent.sub_type,
        vec![codec::SUB_TYPE_QUOTE],
        "a ticker subscription is about quotes and nothing else"
    );
    assert!(
        !sent.is_first_push,
        "the baseline is the pull's, so nothing is asked of the venue's first push"
    );

    let unsubscribe: UnsubscribeRequest = fixture.body_of(codec::UNSUBSCRIBE);
    assert_eq!(unsubscribe.symbol, vec![symbol.as_str().to_owned()]);
    assert_eq!(unsubscribe.sub_type, vec![codec::SUB_TYPE_QUOTE]);
    assert!(!unsubscribe.unsub_all);

    let commands = fixture.commands();
    let pulled = commands
        .iter()
        .position(|command| *command == codec::QUERY_SECURITY_QUOTE)
        .expect("the subscription read its baseline");
    let registered = commands
        .iter()
        .position(|command| *command == codec::SUBSCRIBE)
        .expect("the subscription registered");
    assert!(
        pulled < registered,
        "the baseline was pulled before the subscription was registered: {commands:?}"
    );
}

/// A baseline the venue refuses fails the whole subscription, and no subscribe frame is
/// sent for it: a stream that could not be given a snapshot would be a stream whose first
/// event depends on the market happening to be busy.
#[tokio::test]
async fn a_subscription_that_cannot_read_its_baseline_never_registers() {
    let reference = listing().await;
    let fixture = LongbridgeFixture::install_with(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
        CandleAnswer::default(),
        QuoteAnswer {
            status: 1,
            quotes: Vec::new(),
        },
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;

    let error = subscribe_ticker(&context, &session, &reference)
        .await
        .expect_err("a refused baseline is a refused subscription");

    assert!(
        matches!(
            &error,
            MarketParticipantError::Rejected { code, message, .. }
                if code == "3000000" && message == SUBSCRIPTION_REFUSAL_MESSAGE
        ),
        "{error}"
    );
    assert_eq!(
        fixture
            .settled_frames()
            .await
            .iter()
            .map(|frame| frame.command)
            .collect::<Vec<_>>(),
        vec![
            codec::COMMAND_CODE_AUTH,
            codec::QUERY_USER_QUOTE_PROFILE,
            codec::QUERY_SECURITY_QUOTE
        ],
        "the venue was asked for the baseline and never for a subscription"
    );
}

/// The first event of a subscription is the snapshot the wire pulled, and it arrives
/// without the venue pushing anything at all.
///
/// This is the case the two-field state exists for: a stream that only kept the merge
/// baseline would have nothing to answer with until the venue sent a push, and a quiet
/// market would leave it silent.
#[tokio::test]
async fn a_subscription_answers_its_baseline_before_any_push() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(vec![quote(&symbol)]).await;
    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;

    let handle = subscribe_ticker(&context, &session, &reference)
        .await
        .expect("the fixture answers the baseline and accepts the subscription");
    let events = drain(handle, 1, &fixture).await;

    let ProtocolEvent::Ticker(baseline) = &events[0] else {
        panic!("a ticker stream answers with tickers");
    };
    assert_eq!(baseline.listing, reference.key);
    assert_eq!(baseline.data.close, price("105.00"));
    assert_eq!(baseline.data.open, price("100.00"));
    assert_eq!(baseline.data.high, Some(price("110.00")));
    assert_eq!(baseline.data.low, Some(price("90.00")));
    assert_eq!(
        baseline.data.traded_quantity,
        Some(Quantity::from(1_000_u64))
    );
    assert_eq!(
        baseline.data.traded_notional,
        Some(Quantity::from(105_000_u64))
    );
    assert_eq!(
        baseline.data.monotonic_id, None,
        "the baseline is the pull's own snapshot, which carries no sequence"
    );
}

/// A push states only the fields that changed, and the snapshot it produces is that push
/// merged into the baseline.
///
/// All three frames are what makes this case discriminating. A frame that states only the
/// last price leaves the volume at the decoded default of zero, which the correct merge
/// carries over and a merge that took every field would zero — so the first frame is what
/// tells "keep what was not stated" from "take everything". The second frame states a
/// volume and nothing else, which is what tells the other direction: the correct merge
/// adopts a volume that moved, and a merge that kept the baseline unconditionally would
/// not. The third does the same for the text fields, whose "not stated" spelling is the
/// empty string rather than a zero.
#[tokio::test]
async fn a_push_merges_into_the_baseline_rather_than_replacing_it() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(vec![quote(&symbol)]).await;
    // Every frame is queued before the subscription exists: the fixture delivers the
    // queue once the subscribe frame arrives, so a case that queued one frame after
    // reading another could not say when the venue sent it.
    let mut last_price_only = push(&symbol, 1);
    last_price_only.last_done = "106.00".to_owned();
    fixture.push_quote(&last_price_only);
    let mut volume_only = push(&symbol, 2);
    volume_only.volume = 2_000;
    fixture.push_quote(&volume_only);
    let mut turnover_only = push(&symbol, 3);
    turnover_only.turnover = "210000.00".to_owned();
    fixture.push_quote(&turnover_only);

    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;
    let handle = subscribe_ticker(&context, &session, &reference)
        .await
        .expect("the fixture answers the baseline and accepts the subscription");
    let events = drain(handle, 4, &fixture).await;

    let merged = ticker_of(&events[1]);
    assert_eq!(
        merged.close,
        price("106.00"),
        "the frame stated a last price, so it is adopted"
    );
    assert_eq!(
        merged.open,
        price("100.00"),
        "and the fields it did not state keep the baseline's values"
    );
    assert_eq!(merged.high, Some(price("110.00")));
    assert_eq!(merged.low, Some(price("90.00")));
    assert_eq!(
        merged.traded_quantity,
        Some(Quantity::from(1_000_u64)),
        "a volume of zero is the decoded default rather than a stated zero, so the \
         baseline's volume stands"
    );
    assert_eq!(
        merged.traded_notional,
        Some(Quantity::from(105_000_u64)),
        "and so does the turnover the frame left empty"
    );
    assert_eq!(merged.monotonic_id, Some(1), "the frame's own sequence");

    let volume_moved = ticker_of(&events[2]);
    assert_eq!(
        volume_moved.traded_quantity,
        Some(Quantity::from(2_000_u64)),
        "a volume that moved is adopted, which is the other half of the zero rule"
    );
    assert_eq!(
        volume_moved.close,
        price("106.00"),
        "and the last price the previous frame stated still stands"
    );
    assert_eq!(volume_moved.monotonic_id, Some(2));

    let turnover_moved = ticker_of(&events[3]);
    assert_eq!(
        turnover_moved.traded_notional,
        Some(Quantity::from(210_000_u64)),
        "a text field that is not empty is a field the venue stated"
    );
    assert_eq!(
        turnover_moved.traded_quantity,
        Some(Quantity::from(2_000_u64))
    );
    assert_eq!(turnover_moved.monotonic_id, Some(3));
}

/// One connection publishes every subscription's frames to every subscription, so a quote
/// of another symbol is not this subscription's and produces no event — while the next
/// frame, which is this one's, is read normally. A stream that stopped at the foreign
/// frame would be a stream that one other subscription could end.
#[tokio::test]
async fn another_symbols_push_is_skipped_and_the_stream_reads_on() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(vec![quote(&symbol)]).await;
    let mut foreign = push("MSFT.US", 1);
    foreign.last_done = "999.00".to_owned();
    fixture.push_quote(&foreign);
    let mut own = push(&symbol, 2);
    own.last_done = "107.00".to_owned();
    fixture.push_quote(&own);

    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;
    let handle = subscribe_ticker(&context, &session, &reference)
        .await
        .expect("the fixture answers the baseline and accepts the subscription");
    let events = drain(handle, 2, &fixture).await;

    assert_eq!(
        events.len(),
        2,
        "the baseline and this subscription's own frame, and nothing for the other symbol"
    );
    let ours = ticker_of(&events[1]);
    assert_eq!(
        ours.close,
        price("107.00"),
        "the foreign price is not on this listing's row"
    );
    assert_eq!(ours.monotonic_id, Some(2));
}

/// A push of a command this build does not read is skipped in silence: the venue pushes
/// its order book on the same connection, and a subscription that read it as a quote would
/// be reading another command's body.
#[tokio::test]
async fn a_push_of_another_command_is_skipped() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(vec![quote(&symbol)]).await;
    fixture.push_depth(&PushDepth {
        symbol: symbol.as_str().to_owned(),
        sequence: 1,
        ask: Vec::new(),
        bid: Vec::new(),
    });
    let mut own = push(&symbol, 2);
    own.last_done = "108.00".to_owned();
    fixture.push_quote(&own);

    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;
    let handle = subscribe_ticker(&context, &session, &reference)
        .await
        .expect("the fixture answers the baseline and accepts the subscription");
    let events = drain(handle, 2, &fixture).await;

    assert_eq!(
        ticker_of(&events[1]).close,
        price("108.00"),
        "the depth frame produced no event, and the quote after it was read"
    );
}

/// A subscription the venue refuses ends the stream with the venue's own words: the
/// subscribe answer is not a pending request, so this is the one place it is read.
#[tokio::test]
async fn a_refused_subscription_ends_the_stream_with_the_venues_own_words() {
    let reference = listing().await;
    let symbol = symbol_of(&reference);
    let fixture = fixture_serving(vec![quote(&symbol)]).await;
    let context = execution(&fixture, credentials(), CapabilityId::Tickers);
    let session = session(&context).await;

    let handle = subscribe_ticker(&context, &session, &reference)
        .await
        .expect("the baseline is granted even though the subscription is refused");
    // The subscribe frame is written by the connection's own actor rather than by the
    // caller, so the refusal is set after the handle exists.
    fixture.serve_quotes(QuoteAnswer {
        status: 0,
        quotes: vec![quote(&symbol)],
    });
    fixture.serve_candles(CandleAnswer {
        status: 0,
        candles: Vec::new(),
        subscribe_status: 1,
    });

    let mut stream = handle.into_stream();
    let mut answer = None;
    for _ in 0..3 {
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        match next {
            Ok(Some(Ok(ProtocolEvent::Ticker(_)))) => continue,
            Ok(Some(Ok(other))) => panic!(
                "a ticker stream answers with tickers: {}",
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
    // The fixture answers the subscribe frame from the answer it holds when the frame
    // arrives, so the refusal has to be in place before it is registered; the settled
    // frame log is what says the frame did arrive.
    assert!(
        fixture.commands().contains(&codec::SUBSCRIBE),
        "the subscription was registered"
    );
}
