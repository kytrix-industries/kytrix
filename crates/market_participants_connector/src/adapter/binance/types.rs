use std::{fmt, marker::PhantomData};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{IgnoredAny, SeqAccess, Visitor},
};
use smol_str::SmolStr;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BinanceErrorResponse {
    pub code: i32,
    pub msg: SmolStr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BinanceSubscriptionArgs {
    pub method: SmolStr,
    pub params: Vec<SmolStr>,
    pub id: u64,
}

#[derive(Clone, Debug)]
pub struct BinanceKlineRow {
    pub open_time: i64,
    pub open: SmolStr,
    pub high: SmolStr,
    pub low: SmolStr,
    pub close: SmolStr,
    pub volume: SmolStr,
}

impl<'de> Deserialize<'de> for BinanceKlineRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RowVisitor(PhantomData<fn() -> BinanceKlineRow>);
        impl<'de> Visitor<'de> for RowVisitor {
            type Value = BinanceKlineRow;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Binance kline array")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let open_time = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing kline open time"))?;
                let open = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing kline open"))?;
                let high = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing kline high"))?;
                let low = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing kline low"))?;
                let close = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing kline close"))?;
                let volume = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing kline volume"))?;
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BinanceKlineRow {
                    open_time,
                    open,
                    high,
                    low,
                    close,
                    volume,
                })
            }
        }
        deserializer.deserialize_seq(RowVisitor(PhantomData))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct BinanceTickerWire {
    pub symbol: SmolStr,
    #[serde(rename = "openPrice")]
    pub open_price: SmolStr,
    #[serde(rename = "highPrice")]
    pub high_price: Option<SmolStr>,
    #[serde(rename = "lowPrice")]
    pub low_price: Option<SmolStr>,
    #[serde(rename = "lastPrice")]
    pub last_price: SmolStr,
    pub volume: SmolStr,
    #[serde(rename = "quoteVolume")]
    pub quote_volume: SmolStr,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BinanceOrderBookWire {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: Option<u64>,
    #[serde(default, rename = "bids")]
    pub bids: Vec<BinanceLevelRow>,
    #[serde(default, rename = "asks")]
    pub asks: Vec<BinanceLevelRow>,
}

#[derive(Clone, Debug)]
pub struct BinanceLevelRow {
    pub price: SmolStr,
    pub quantity: SmolStr,
}

impl<'de> Deserialize<'de> for BinanceLevelRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LevelVisitor(PhantomData<fn() -> BinanceLevelRow>);
        impl<'de> Visitor<'de> for LevelVisitor {
            type Value = BinanceLevelRow;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Binance order-book level array")
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let price = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing level price"))?;
                let quantity = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::custom("missing level quantity"))?;
                Ok(BinanceLevelRow { price, quantity })
            }
        }
        deserializer.deserialize_seq(LevelVisitor(PhantomData))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct BinanceRecentTradeWire {
    pub id: u64,
    pub price: SmolStr,
    pub qty: SmolStr,
    pub time: i64,
    #[serde(rename = "isBuyerMaker")]
    pub buyer_is_maker: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BinanceOrderResult {
    #[serde(rename = "orderId")]
    pub order_id: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{BinanceErrorResponse, BinanceSubscriptionArgs};

    /// The subscription frame the venue expects is asserted rather than derived:
    /// Binance rejects a stream name that is not a bare string.
    #[test]
    fn a_subscription_frame_carries_the_stream_names_and_the_request_id() {
        let args = BinanceSubscriptionArgs {
            method: "SUBSCRIBE".into(),
            params: vec!["btcusdt@kline_1m".into()],
            id: 7,
        };
        let encoded = serde_json::to_string(&args).expect("the frame should encode");

        assert_eq!(
            encoded,
            r#"{"method":"SUBSCRIBE","params":["btcusdt@kline_1m"],"id":7}"#
        );
    }

    /// The venue's error body is the code and message it sent, verbatim.
    #[test]
    fn an_error_response_keeps_the_venue_code_and_message() {
        let error: BinanceErrorResponse =
            serde_json::from_str(r#"{"code":-1121,"msg":"Invalid symbol."}"#)
                .expect("the fixture error should decode");

        assert_eq!(error.code, -1121);
        assert_eq!(error.msg, "Invalid symbol.");
    }
}
