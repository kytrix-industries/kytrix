//! How one measurement is rendered for the run's output.
//!
//! Every case prints what it measured, so these are the words the prints are made of:
//! the market's own clock, a candle's summary line, and the venue's name for a session.

use longbridge_proto::quote::{Candlestick, PushQuote, PushQuoteTag, TradeSession};

/// Where one instant's bucket starts on the ordinary table, as the case states it.
pub(crate) use crate::grid::bucket_start;
use crate::grid::{market_local, now_millis, session_of};

/// `HH:MM` of one epoch-millisecond instant, in the market's zone.
pub fn market_clock(millis: i64) -> String {
    market_local(millis).strftime("%H:%M").to_string()
}

/// The venue's name for one session number.
pub fn session_name(session: i32) -> &'static str {
    match TradeSession::try_from(session) {
        Ok(TradeSession::NormalTrade) => "normal",
        Ok(TradeSession::PreTrade) => "pre",
        Ok(TradeSession::PostTrade) => "post",
        Ok(TradeSession::OvernightTrade) => "overnight",
        Err(_) => "unknown",
    }
}

/// The venue's name for the session one instant falls in, of the ordinary table.
///
/// An instant inside no window has no session number of its own — the venue's own
/// numbering has no value for one — so the label says so rather than borrowing a
/// session's name.
pub fn session_label(millis: i64) -> &'static str {
    session_of(&market_local(millis))
        .map(|session| session_name(session as i32))
        .unwrap_or("outside every session")
}

/// Reports whether the market is inside its regular session at one instant.
///
/// The `104` clause of the subscription case turns on this and on nothing else: the
/// venue's own series showed a trade push only where there are trades to push, so the
/// assertion is judged in the regular session and reported, not silently passed, out
/// of it.
pub fn in_regular_session(millis: i64) -> bool {
    session_of(&market_local(millis)) == Some(TradeSession::NormalTrade)
}

/// What one candlestick list measured, for a case that prints it.
pub fn candle_summary(candles: &[Candlestick], up_to: usize) -> Vec<String> {
    candles
        .iter()
        .take(up_to)
        .map(|candle| {
            format!(
                "    t={} ({}) session={} o={} h={} l={} c={} v={}",
                candle.timestamp,
                market_clock(candle.timestamp.saturating_mul(1_000)),
                session_name(candle.trade_session),
                candle.open,
                candle.high,
                candle.low,
                candle.close,
                candle.volume
            )
        })
        .collect()
}

/// Asserts the facts every candlestick answer of this suite must satisfy: the
/// timestamps are ascending, they are seconds restated as milliseconds, and the
/// venue returned no more candles than were asked for.
pub fn assert_candle_list(case: &str, candles: &[Candlestick], count: i32) {
    for pair in candles.windows(2) {
        assert!(
            pair[0].timestamp <= pair[1].timestamp,
            "{case}: the venue returns candles in ascending order; {} came before {}",
            pair[0].timestamp,
            pair[1].timestamp
        );
    }
    for candle in candles {
        assert!(
            candle.timestamp > 0,
            "{case}: a timestamp is a positive Unix second"
        );
        let millis = candle.timestamp.saturating_mul(1_000);
        assert!(
            millis < now_millis() + 86_400_000,
            "{case}: timestamp {} is a Unix second, not a millisecond",
            candle.timestamp
        );
        assert!(
            candle.timestamp < 100_000_000_000,
            "{case}: timestamp {} would be a millisecond value, not a second",
            candle.timestamp
        );
    }
    assert!(
        candles.len() <= usize::try_from(count).expect("a positive count"),
        "{case}: the venue returned {} candles for a count of {count}",
        candles.len()
    );
}

/// Every fact one `101` snapshot of one subscribed symbol must satisfy on its own.
///
/// These are the facts the case can judge whether or not the market is open: the
/// snapshot is about the symbol that was subscribed, it is filed under a session the
/// venue names, and the fields the venue states are present. What it is *not* is
/// evidence about a trade push, which is why the case states which of the two it saw.
pub fn assert_quote_snapshot(case: &str, quote: &PushQuote, symbol: &str) {
    assert_eq!(
        quote.symbol, symbol,
        "{case}: a snapshot is about the symbol that was subscribed"
    );
    assert!(
        TradeSession::try_from(quote.trade_session).is_ok(),
        "{case}: a snapshot states one of the venue's sessions, not {}",
        quote.trade_session
    );
    assert!(
        matches!(
            quote.tag,
            value if value == PushQuoteTag::Normal as i32 || value == PushQuoteTag::Eod as i32
        ),
        "{case}: a snapshot states a tag the venue publishes, not {}",
        quote.tag
    );
    assert!(
        quote.timestamp > 0,
        "{case}: a snapshot states a positive Unix second"
    );
    assert!(
        !quote.last_done.is_empty(),
        "{case}: a snapshot states the last price it is a snapshot of"
    );
    assert!(
        quote.volume >= 0,
        "{case}: the day's cumulative volume is not negative"
    );
}
