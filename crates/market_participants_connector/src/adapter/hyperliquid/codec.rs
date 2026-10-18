use bytes::Bytes;
use market_participants_model::listing::route::VenueRoute;
use serde::Serialize;
use smol_str::SmolStr;

use super::types::{
    HyperliquidCandleSnapshotRequest, HyperliquidInfoRequest, HyperliquidPostRequest,
    HyperliquidSubscribe, HyperliquidSubscription, HyperliquidWirePost,
};
use crate::{error::MarketParticipantError, market_participants::model::TimeFrame};

/// Hyperliquid candle intervals, in the spelling its API expects.
///
/// Hyperliquid candles follow its funding clock: eight-hour candles exist and
/// six-hour ones do not.
const HYPERLIQUID_INTERVALS: &[(TimeFrame, &str)] = &[
    (TimeFrame::Minute1, "1m"),
    (TimeFrame::Minute3, "3m"),
    (TimeFrame::Minute5, "5m"),
    (TimeFrame::Minute15, "15m"),
    (TimeFrame::Minute30, "30m"),
    (TimeFrame::Hour1, "1h"),
    (TimeFrame::Hour2, "2h"),
    (TimeFrame::Hour4, "4h"),
    (TimeFrame::Hour8, "8h"),
    (TimeFrame::Hour12, "12h"),
    (TimeFrame::Day1, "1d"),
    (TimeFrame::Day3, "3d"),
    (TimeFrame::Week1, "1w"),
    (TimeFrame::Month1, "1M"),
];
#[derive(Clone, Copy, Debug, Default)]
pub struct HyperliquidCodec;
impl HyperliquidCodec {
    /// Extracts the request ID from a Hyperliquid `post` response.
    ///
    /// Subscription events and other channels may contain unrelated `id`
    /// fields, so only the documented `channel == "post"` envelope is used.
    pub fn extract_response_id(payload: &[u8]) -> Option<u64> {
        let response = serde_json::from_slice::<HyperliquidResponseIdEnvelope>(payload).ok()?;
        (response.channel == "post").then_some(response.data.id)
    }

    pub fn encode<T: Serialize>(&self, value: &T) -> Result<Bytes, MarketParticipantError> {
        serde_json::to_vec(value).map(Bytes::from).map_err(|error| {
            MarketParticipantError::ProtocolError {
                operation: "encode Hyperliquid message".into(),
                reason: smol_str::format_smolstr!("{}", error),
                raw_data: None,
            }
        })
    }
    #[cfg(test)]
    pub(crate) fn decode<T: serde::de::DeserializeOwned>(
        &self,
        payload: &[u8],
    ) -> Result<T, MarketParticipantError> {
        serde_json::from_slice(payload).map_err(|_| MarketParticipantError::DecodeFailed {
            operation: "decode Hyperliquid message".into(),
            raw_data: Bytes::copy_from_slice(payload),
        })
    }

    pub fn candle_snapshot_request(
        &self,
        coin: impl Into<SmolStr>,
        interval: impl Into<SmolStr>,
        start_time: i64,
        end_time: i64,
    ) -> HyperliquidInfoRequest {
        HyperliquidInfoRequest::CandleSnapshot {
            req: HyperliquidCandleSnapshotRequest {
                coin: coin.into(),
                interval: interval.into(),
                start_time,
                end_time,
            },
        }
    }
    pub fn info_post(
        &self,
        request: HyperliquidInfoRequest,
        id: u64,
    ) -> HyperliquidWirePost<HyperliquidInfoRequest> {
        HyperliquidWirePost {
            method: "post".into(),
            id,
            request: HyperliquidPostRequest {
                r#type: "info".into(),
                payload: request,
            },
        }
    }
    pub fn action_post<T: serde::Serialize>(&self, action: T, id: u64) -> HyperliquidWirePost<T> {
        HyperliquidWirePost {
            method: "post".into(),
            id,
            request: HyperliquidPostRequest {
                r#type: "action".into(),
                payload: action,
            },
        }
    }
    pub fn recent_trades_request(
        &self,
        coin: impl Into<smol_str::SmolStr>,
    ) -> HyperliquidInfoRequest {
        HyperliquidInfoRequest::RecentTrades { coin: coin.into() }
    }
    pub fn candle_subscription(
        &self,
        coin: impl Into<smol_str::SmolStr>,
        interval: impl Into<smol_str::SmolStr>,
    ) -> HyperliquidSubscribe<HyperliquidSubscription> {
        HyperliquidSubscribe {
            method: "subscribe".into(),
            subscription: HyperliquidSubscription::Candle {
                coin: coin.into(),
                interval: interval.into(),
            },
        }
    }
    pub fn user_fills_subscription(
        &self,
        user: impl Into<smol_str::SmolStr>,
    ) -> HyperliquidSubscribe<HyperliquidSubscription> {
        HyperliquidSubscribe {
            method: "subscribe".into(),
            subscription: HyperliquidSubscription::UserFills { user: user.into() },
        }
    }
    pub fn user_ledger_subscription(
        &self,
        user: impl Into<smol_str::SmolStr>,
    ) -> HyperliquidSubscribe<HyperliquidSubscription> {
        HyperliquidSubscribe {
            method: "subscribe".into(),
            subscription: HyperliquidSubscription::UserNonFundingLedgerUpdates {
                user: user.into(),
            },
        }
    }
    /// The subscription that carries every market number one instrument's ticker
    /// reports.
    ///
    /// An asset context is the only channel that publishes the change and the
    /// volume of a day next to the price, and it serves spot and perpetual
    /// instruments alike.
    pub fn ticker_subscription(
        &self,
        coin: impl Into<SmolStr>,
    ) -> HyperliquidSubscribe<HyperliquidSubscription> {
        HyperliquidSubscribe {
            method: "subscribe".into(),
            subscription: HyperliquidSubscription::ActiveAssetCtx { coin: coin.into() },
        }
    }
    pub fn trades_subscription(
        &self,
        coin: impl Into<smol_str::SmolStr>,
    ) -> HyperliquidSubscribe<HyperliquidSubscription> {
        HyperliquidSubscribe {
            method: "subscribe".into(),
            subscription: HyperliquidSubscription::Trades { coin: coin.into() },
        }
    }
    pub fn order_book_subscription(
        &self,
        coin: impl Into<smol_str::SmolStr>,
    ) -> HyperliquidSubscribe<HyperliquidSubscription> {
        HyperliquidSubscribe {
            method: "subscribe".into(),
            subscription: HyperliquidSubscription::L2Book { coin: coin.into() },
        }
    }
    /// The interval Hyperliquid expects for `timeframe` on `route`.
    ///
    /// The set is Hyperliquid's own candle set: an interval this route does not
    /// publish is refused here rather than by the venue.
    pub fn encode_timeframe(
        &self,
        route: &VenueRoute,
        timeframe: TimeFrame,
    ) -> Result<&'static str, MarketParticipantError> {
        HYPERLIQUID_INTERVALS
            .iter()
            .find(|(interval, _)| *interval == timeframe)
            .map(|(_, spelling)| *spelling)
            .ok_or_else(|| MarketParticipantError::UnsupportedTimeFrame {
                route: route.clone(),
                timeframe: SmolStr::new(timeframe.label()),
            })
    }
    #[cfg(test)]
    pub(crate) fn decode_candle(
        &self,
        wire: super::types::HyperliquidCandle,
    ) -> Result<crate::adapter::NativeCandlestick, MarketParticipantError> {
        let candle = crate::adapter::NativeCandlestick {
            timestamp: wire.t,
            open: wire
                .o
                .parse()
                .map_err(|_| MarketParticipantError::DecodeFailed {
                    operation: "decode Hyperliquid candle".into(),
                    raw_data: Bytes::new(),
                })?,
            high: wire
                .h
                .parse()
                .map_err(|_| MarketParticipantError::DecodeFailed {
                    operation: "decode Hyperliquid candle".into(),
                    raw_data: Bytes::new(),
                })?,
            low: wire
                .l
                .parse()
                .map_err(|_| MarketParticipantError::DecodeFailed {
                    operation: "decode Hyperliquid candle".into(),
                    raw_data: Bytes::new(),
                })?,
            close: wire
                .c
                .parse()
                .map_err(|_| MarketParticipantError::DecodeFailed {
                    operation: "decode Hyperliquid candle".into(),
                    raw_data: Bytes::new(),
                })?,
            volume: wire
                .v
                .parse()
                .map_err(|_| MarketParticipantError::DecodeFailed {
                    operation: "decode Hyperliquid candle".into(),
                    raw_data: Bytes::new(),
                })?,
        };
        Ok(candle)
    }
}

#[derive(serde::Deserialize)]
struct HyperliquidResponseIdEnvelope {
    channel: SmolStr,
    data: HyperliquidResponseIdData,
}

#[derive(serde::Deserialize)]
struct HyperliquidResponseIdData {
    id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::hyperliquid::HyperliquidCandle;

    #[test]
    fn hyperliquid_codec_matches_info_post_and_candle_shapes() {
        let codec = HyperliquidCodec;
        let request = codec.candle_snapshot_request("BTC", "1h", 1, 2);
        let request = codec.info_post(request, 7);
        let request_wire = serde_json::to_string(&request).unwrap();
        assert!(request_wire.contains("\"method\":\"post\""));
        assert!(request_wire.contains("\"id\":7"));
        assert!(request_wire.contains("\"type\":\"info\""));
        assert!(request_wire.contains("\"candleSnapshot\""));

        let fills = codec.user_fills_subscription("0x0000000000000000000000000000000000000001");
        let ledger = codec.user_ledger_subscription("0x0000000000000000000000000000000000000001");
        assert_eq!(
            serde_json::to_string(&codec.ticker_subscription("BTC")).unwrap(),
            r#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}"#
        );
        assert_eq!(
            serde_json::to_string(&fills).unwrap(),
            r#"{"method":"subscribe","subscription":{"type":"userFills","user":"0x0000000000000000000000000000000000000001"}}"#
        );
        assert_eq!(
            serde_json::to_string(&ledger).unwrap(),
            r#"{"method":"subscribe","subscription":{"type":"userNonFundingLedgerUpdates","user":"0x0000000000000000000000000000000000000001"}}"#
        );

        let encoded = codec.encode(&request).unwrap();
        let decoded = codec
            .decode::<crate::adapter::hyperliquid::HyperliquidWirePost<
                crate::adapter::hyperliquid::HyperliquidInfoRequest,
            >>(&encoded)
            .unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            request_wire,
            r#"{"method":"post","id":7,"request":{"type":"info","payload":{"type":"candleSnapshot","req":{"coin":"BTC","interval":"1h","startTime":1,"endTime":2}}}}"#
        );

        let candle = codec
            .decode_candle(HyperliquidCandle {
                t: 1_700_000_000_000,
                o: "100".into(),
                h: "110".into(),
                l: "90".into(),
                c: "105".into(),
                v: "12.5".into(),
            })
            .unwrap();
        assert_eq!(candle.timestamp, 1_700_000_000_000);
        assert_eq!(candle.close.to_string(), "105");
        assert_eq!(candle.volume.to_string(), "12.5");
    }

    #[test]
    fn extracts_nested_post_response_id_only() {
        assert_eq!(
            HyperliquidCodec::extract_response_id(
                br#"{"channel":"post","data":{"id":42,"response":{}}}"#
            ),
            Some(42)
        );
        assert_eq!(
            HyperliquidCodec::extract_response_id(
                br#"{"channel":"subscription","data":{"id":43}}"#
            ),
            None
        );
        assert_eq!(
            HyperliquidCodec::extract_response_id(br#"{"data":{"id":44}}"#),
            None
        );
    }
}
