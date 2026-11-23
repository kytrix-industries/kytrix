use longbridge_proto::quote::{Candlestick, PushQuote, PushQuoteTag, TradeSession};

pub(crate) use crate::grid::bucket_start;
use crate::grid::{market_local, now_millis, session_of};

pub fn market_clock(millis: i64) -> String {
    market_local(millis).strftime("%H:%M").to_string()
}

pub fn session_name(session: i32) -> &'static str {
    match TradeSession::try_from(session) {
        Ok(TradeSession::NormalTrade) => "normal",
        Ok(TradeSession::PreTrade) => "pre",
        Ok(TradeSession::PostTrade) => "post",
        Ok(TradeSession::OvernightTrade) => "overnight",
        Err(_) => "unknown",
    }
}

pub fn session_label(millis: i64) -> &'static str {
    session_of(&market_local(millis))
        .map(|session| session_name(session as i32))
        .unwrap_or("outside every session")
}

pub fn in_regular_session(millis: i64) -> bool {
    session_of(&market_local(millis)) == Some(TradeSession::NormalTrade)
}

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
