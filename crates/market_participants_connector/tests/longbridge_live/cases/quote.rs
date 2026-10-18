//! The real-time quote cases: the pull (cmd 11) and the incremental semantics of the
//! subscription's pushes (cmd 6, push 101).
//!
//! These two are what the watch list's ticker path stands on. The pull is the snapshot a
//! row is first drawn from, and the pushes are what move it afterwards; the venue's own
//! documentation says a push fills in only the fields that changed, and this module is
//! where that statement is measured rather than believed.

use longbridge_proto::quote::{
    MultiSecurityRequest, PushQuote, SecurityQuoteResponse, SubType, SubscribeRequest,
};
use prost::Message as _;
use tokio::time::timeout;

use crate::{
    bounds::{CASE_TIMEOUT, PUSH_WINDOW},
    client::chosen_transport,
    credentials::Credentials,
    describe::{in_regular_session, market_clock},
    grid::now_millis,
    skip_without_credentials,
    socket::LiveSocket,
    wire::{Failure, PUSH_REALTIME_QUOTE, QUERY_SECURITY_QUOTE, SUBSCRIBE},
};

/// How recently a timestamp read as seconds or as milliseconds has to fall.
const RECENT: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Case 18 — what the real-time quote command answers (§3.3, §9.1 items 1 and 2).
///
/// The design reads the venue's own documentation for the field meanings and the vendor's
/// proto for their numbers. What documentation cannot answer is the unit of `timestamp`
/// and the dimension of `volume` against the currency of `turnover`, so both are measured
/// here: the timestamp is resolved to whichever of its two readings falls in the recent
/// past, and `turnover / volume` is compared against `last_done` on the one occasion the
/// comparison means anything.
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_18_the_real_time_quote_command_reads_back() {
    let case = "case 18 (cmd 11 quote)";
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

        let answer = match socket
            .request::<SecurityQuoteResponse>(
                QUERY_SECURITY_QUOTE,
                &MultiSecurityRequest {
                    symbol: vec![symbol.clone()],
                },
            )
            .await
        {
            Ok(answer) => answer,
            // An account without US market data is not a broken case: the suite has no
            // permission to measure, and says so rather than failing the run.
            Err(Failure::Refused { code, message, .. }) => {
                println!(
                    "SKIP {case}: the venue refused cmd 11 with code {code}: {message} (the \
                     account's US market-data permission is the design's own precondition, §3.6)"
                );
                socket.shutdown().await;
                return;
            }
            Err(other) => panic!("{case}: the real-time quote failed: {other}"),
        };
        socket.shutdown().await;

        assert_eq!(
            answer.secu_quote.len(),
            1,
            "{case}: one symbol was asked about, so one quote comes back"
        );
        let quote = &answer.secu_quote[0];
        println!(
            "{case}: symbol={} last_done={} prev_close={} open={} high={} low={} volume={} \
             turnover={} trade_status={}",
            quote.symbol,
            quote.last_done,
            quote.prev_close,
            quote.open,
            quote.high,
            quote.low,
            quote.volume,
            quote.turnover,
            quote.trade_status
        );
        assert_eq!(
            quote.symbol, symbol,
            "{case}: the answer is about the symbol that was asked about"
        );

        for (name, value) in [
            ("last_done", &quote.last_done),
            ("open", &quote.open),
            ("high", &quote.high),
            ("low", &quote.low),
        ] {
            assert!(
                !value.is_empty() && value.parse::<f64>().is_ok(),
                "{case}: {name} is a non-empty decimal, and it is {value:?}"
            );
        }

        // The unit of `timestamp` is not documented (§9.1 item 1), so both readings are
        // printed and the assertion is the one fact either reading must satisfy: exactly
        // one of them is recent.
        let now = now_millis();
        let as_seconds = quote.timestamp.saturating_mul(1_000);
        let as_millis = quote.timestamp;
        let seconds_recent = within(as_seconds, now);
        let millis_recent = within(as_millis, now);
        println!(
            "  timestamp={} read as seconds is {} ({} ET, inside the recent window: \
             {seconds_recent}); read as milliseconds it is {} ({millis_recent})",
            quote.timestamp,
            as_seconds,
            market_clock(as_seconds),
            as_millis
        );
        assert!(
            seconds_recent ^ millis_recent,
            "{case}: a timestamp has exactly one reading that falls inside the last {} days \
             (§9.1 item 1); seconds={seconds_recent}, millis={millis_recent}",
            RECENT.as_secs() / 86_400
        );
        println!(
            "  §9.1 item 1 measured: the quote's timestamp is unix {}",
            if seconds_recent {
                "seconds"
            } else {
                "milliseconds"
            }
        );

        assert!(
            quote.volume >= 0,
            "{case}: a cumulative volume is not negative"
        );
        assert!(
            quote.turnover.parse::<f64>().is_ok(),
            "{case}: turnover is a decimal, and it is {:?}",
            quote.turnover
        );

        // The dimension evidence (§9.1 item 2) only means anything on the day's own
        // cumulative figures inside the regular session: `last_done` may come from the
        // overnight or post session, whose price divided by the day's cumulative VWAP can
        // reasonably be far from one. Every precondition is stated, and a run that does
        // not meet one reports that it could not judge the clause rather than passing it.
        let volume = quote.volume;
        let turnover = quote.turnover.parse::<f64>().unwrap_or_default();
        let last = quote.last_done.parse::<f64>().unwrap_or_default();
        if volume <= 0 || turnover <= 0.0 {
            println!(
                "  SKIP the dimension check: volume={volume} turnover={turnover}, and a ratio of \
                 the day's own cumulative figures needs both of them positive"
            );
        } else if !in_regular_session(now) {
            println!(
                "  SKIP the dimension check: now is {} ET, outside the regular session, and \
                 `last_done` may be a pre, post or overnight price while volume and turnover \
                 are the day's cumulative totals",
                market_clock(now)
            );
        } else if !crate::cases::push::trading_day(&credentials, case, now).await {
            println!(
                "  SKIP the dimension check: the venue's calendar does not name {} a trading day, \
                 so the day's cumulative figures are not the ones this price belongs to",
                crate::grid::market_date(now)
            );
        } else {
            let per_share = turnover / volume as f64;
            let ratio = per_share / last;
            println!(
                "  dimension check: turnover/volume = {per_share:.6} per share against last_done = \
                 {last}, a ratio of {ratio:.4}"
            );
            if (0.5..=2.0).contains(&ratio) {
                println!(
                    "  §9.1 item 2 measured: volume's dimension and turnover's currency agree \
                     with last_done (quantity x unit price = amount)"
                );
            } else {
                println!(
                    "  §9.1 item 2 INCONSISTENT with the assumption this case states: \
                     turnover/volume is {per_share:.6} per share against last_done = {last}, a \
                     ratio of {ratio:.4}, outside [0.5, 2.0] x last_done; §9.1 item 2 needs \
                     updating rather than this bound, and this measurement does not fail the case"
                );
            }
        }
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 19 — a subscription's pushes state only what changed (§3.1, §4.2).
///
/// This is the measurement the adapter's merge exists for. The baseline is pulled with
/// cmd 11 first, exactly as the adapter pulls it, and the subscription asks for no opening
/// push because the adapter asks for none; what the venue then sends is recorded field by
/// field, so the run can report which fields a push leaves at their decoded defaults.
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_19_a_subscription_pushes_only_what_changed() {
    let case = "case 19 (incremental pushes)";
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

        // The baseline, read with the command the adapter reads it with. A venue refusal
        // here is the same situation case 18 skips on: an account without US market data
        // is not a broken case, and the suite says so rather than failing the run.
        let baseline: SecurityQuoteResponse = match socket
            .request(
                QUERY_SECURITY_QUOTE,
                &MultiSecurityRequest {
                    symbol: vec![symbol.clone()],
                },
            )
            .await
        {
            Ok(answer) => answer,
            Err(Failure::Refused { code, message, .. }) => {
                println!(
                    "SKIP {case}: the venue refused cmd 11 with code {code}: {message} (the \
                     account's US market-data permission is the design's own precondition, §3.6)"
                );
                socket.shutdown().await;
                return;
            }
            Err(other) => panic!("{case}: the baseline read failed: {other}"),
        };
        let baseline = baseline
            .secu_quote
            .first()
            .unwrap_or_else(|| panic!("{case}: the baseline read answered no quote"));
        println!(
            "{case}: baseline last_done={} open={} high={} low={} volume={} turnover={}",
            baseline.last_done,
            baseline.open,
            baseline.high,
            baseline.low,
            baseline.volume,
            baseline.turnover
        );

        // The frame the adapter sends: quotes alone, and no opening push, because the
        // baseline was already pulled.
        let subscribe = SubscribeRequest {
            symbol: vec![symbol.clone()],
            sub_type: vec![SubType::Quote as i32],
            is_first_push: false,
        };
        let answer = socket
            .request_raw(SUBSCRIBE, &subscribe.encode_to_vec())
            .await
            .unwrap_or_else(|error| panic!("{case}: the subscription failed: {error}"));
        println!(
            "  cmd 6 ack: command_code={} status={:?} body_len={}",
            answer.command_code,
            answer.status,
            answer.body.len()
        );
        assert_eq!(answer.command_code, SUBSCRIBE);
        assert_eq!(answer.status, Some(0), "{case}: the venue accepted cmd 6");

        let deadline = tokio::time::Instant::now() + PUSH_WINDOW;
        let recorded = socket.drain_pushes(deadline).await;
        socket.shutdown().await;

        let mut quotes = Vec::with_capacity(recorded.len());
        let mut others: Vec<u8> = Vec::with_capacity(2);
        let frames = recorded.len();
        for push in recorded {
            match push.command_code {
                PUSH_REALTIME_QUOTE => match PushQuote::decode(push.body.as_ref()) {
                    Ok(quote) => quotes.push(quote),
                    Err(error) => println!("  a 101 frame did not decode: {error}"),
                },
                other => others.push(other),
            }
        }
        println!(
            "  over {PUSH_WINDOW:?}: {frames} inbound frame(s), {} of them 101, other command \
             codes {others:?}",
            quotes.len()
        );
        assert!(
            quotes.iter().all(|quote| quote.symbol == symbol),
            "{case}: every push must be about the symbol that was subscribed"
        );
        assert!(
            quotes
                .windows(2)
                .all(|pair| pair[0].sequence <= pair[1].sequence),
            "{case}: `sequence` is what this build publishes as the ticker's ordering marker \
             (§4.2 decision 4), so it must not move backwards; the run measured {:?}",
            quotes
                .iter()
                .map(|quote| quote.sequence)
                .collect::<Vec<_>>()
        );

        // Which fields each frame left at the default prost decodes an unset field into.
        let mut empty_open = 0_usize;
        let mut empty_last = 0_usize;
        let mut empty_turnover = 0_usize;
        let mut zero_volume = 0_usize;
        for (index, quote) in quotes.iter().enumerate() {
            let open = quote.open.is_empty();
            let last = quote.last_done.is_empty();
            let turnover = quote.turnover.is_empty();
            let volume = quote.volume == 0;
            empty_open += usize::from(open);
            empty_last += usize::from(last);
            empty_turnover += usize::from(turnover);
            zero_volume += usize::from(volume);
            println!(
                "    101 #{index} sequence={} t={} session={} tag={}: empty open={open} \
                 last_done={last} high={} low={} turnover={turnover}; volume={} (zero: {volume}); \
                 current_volume={}",
                quote.sequence,
                quote.timestamp,
                crate::describe::session_name(quote.trade_session),
                quote.tag,
                quote.high.is_empty(),
                quote.low.is_empty(),
                quote.volume,
                quote.current_volume
            );
        }
        println!(
            "  of {} 101 frame(s): {empty_open} stated no open, {empty_last} stated no last_done, \
             {empty_turnover} stated no turnover, {zero_volume} stated a volume of zero",
            quotes.len()
        );

        if empty_open + empty_last + empty_turnover + zero_volume > 0 {
            println!(
                "  §9.1 item 3 measured: the venue really does send frames with fields left at \
                 their defaults, so §4.2's merge is required rather than defensive"
            );
        } else if quotes.is_empty() {
            println!(
                "  §9.1 item 3 NOT MEASURED: the venue pushed no 101 frame in this window, which \
                 is what a closed market looks like. §4.2's merge stays implemented as the \
                 venue's documentation requires it"
            );
        } else {
            println!(
                "  §9.1 item 3 NOT MEASURED: every frame of this window stated every field. \
                 §4.2's merge stays implemented as the venue's documentation requires it; this \
                 run is not evidence that the frames are always complete"
            );
        }
        if let Some(first) = quotes.first() {
            let complete = !first.open.is_empty()
                && !first.last_done.is_empty()
                && !first.high.is_empty()
                && !first.low.is_empty()
                && !first.turnover.is_empty()
                && first.volume != 0;
            println!(
                "  §9.1 item 4 observed: the first 101 frame of this window is {} complete. The \
                 implementation does not depend on the answer: its baseline is the pull's own \
                 snapshot, which stated open={:?} last_done={:?}",
                if complete { "entirely" } else { "not" },
                baseline.open,
                baseline.last_done
            );
        }
        let zero_levels = quotes
            .iter()
            .filter(|quote| quote.high == "0" || quote.low == "0")
            .count();
        let zero_open = quotes.iter().filter(|quote| quote.open == "0").count();
        let zero_last = quotes.iter().filter(|quote| quote.last_done == "0").count();
        println!(
            "  §9.1 item 5 observed: {zero_levels} frame(s) stated a high or a low of \"0\", \
             {zero_open} stated an open of \"0\" and {zero_last} a last_done of \"0\"; the \
             implementation reads a zero level as no level"
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Reports whether one instant falls inside the recent past.
fn within(millis: i64, now: i64) -> bool {
    let recent = i64::try_from(RECENT.as_millis()).unwrap_or(i64::MAX);
    millis > 0 && millis <= now && now - millis <= recent
}
