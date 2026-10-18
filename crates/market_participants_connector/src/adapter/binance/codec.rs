use bytes::Bytes;
use market_participants_model::{
    identifier::NativeSymbol,
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        route::{VenueListingKey, VenueRoute},
        venue::BinanceProduct,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use smol_str::format_smolstr;

use super::types::BinanceErrorResponse;
use crate::{
    adapter::error::http_response_error,
    error::{MarketParticipantError, OrderErrorKind},
    market_participants::model::TimeFrame,
};

/// Binance klines, in the spelling its API expects.
///
/// Binance publishes no two-day candle, and its second candles exist on the spot
/// market only, so the set is keyed by the scope the request is addressed to.
const BINANCE_INTERVALS: &[(TimeFrame, &str)] = &[
    (TimeFrame::Second1, "1s"),
    (TimeFrame::Minute1, "1m"),
    (TimeFrame::Minute3, "3m"),
    (TimeFrame::Minute5, "5m"),
    (TimeFrame::Minute15, "15m"),
    (TimeFrame::Minute30, "30m"),
    (TimeFrame::Hour1, "1h"),
    (TimeFrame::Hour2, "2h"),
    (TimeFrame::Hour4, "4h"),
    (TimeFrame::Hour6, "6h"),
    (TimeFrame::Hour8, "8h"),
    (TimeFrame::Hour12, "12h"),
    (TimeFrame::Day1, "1d"),
    (TimeFrame::Day3, "3d"),
    (TimeFrame::Week1, "1w"),
    (TimeFrame::Month1, "1M"),
];

#[derive(Clone, Copy, Debug, Default)]
pub struct BinanceCodec;
/// Returns whether Binance publishes the one-second candle on `route`.
///
/// The interval exists on the spot market only: the futures kline endpoint
/// answers `1s` with an invalid-interval error.
fn publishes_second(route: &VenueRoute) -> bool {
    route.scope.as_str() == crate::routing::SPOT_SCOPE
}

impl BinanceCodec {
    /// The product family one Binance scope trades.
    pub fn product(route: &VenueRoute) -> Option<BinanceProduct> {
        match route.scope.as_str() {
            crate::routing::SPOT_SCOPE => Some(BinanceProduct::Spot),
            crate::routing::PERPETUAL_SCOPE => Some(BinanceProduct::Perpetual),
            _ => None,
        }
    }

    /// The listing identity one Binance symbol names on `route`.
    ///
    /// A route-scoped answer names its subject by the venue's symbol alone, and a
    /// Binance symbol is the stable protocol identity of that listing, so the key
    /// is derived from the symbol exactly the way the catalog derives it — there
    /// is no directory an adapter could consult, and an invented key would name a
    /// listing the published catalog does not hold.
    pub fn listing_key(
        route: &VenueRoute,
        product: BinanceProduct,
        symbol: &str,
    ) -> Result<VenueListingKey, MarketParticipantError> {
        let instrument_id =
            NativeSymbol::new(symbol).map_err(|error| MarketParticipantError::ProtocolError {
                operation: "derive Binance listing key".into(),
                reason: format_smolstr!("{}", error),
                raw_data: None,
            })?;
        ListingContinuityKey {
            route: route.clone(),
            terms: ListingContinuityTerms::Binance {
                product,
                instrument_id,
            },
        }
        .listing_key()
        .map_err(|error| MarketParticipantError::ProtocolError {
            operation: "derive Binance listing key".into(),
            reason: format_smolstr!("{}", error),
            raw_data: None,
        })
    }

    /// Extracts the correlation ID from a Binance WebSocket response.
    pub fn extract_response_id(payload: &[u8]) -> Option<u64> {
        serde_json::from_slice::<BinanceResponseId>(payload)
            .ok()
            .and_then(|response| response.id)
    }

    pub fn encode<T: Serialize>(&self, value: &T) -> Result<Bytes, MarketParticipantError> {
        serde_json::to_vec(value).map(Bytes::from).map_err(|error| {
            MarketParticipantError::ProtocolError {
                operation: "encode Binance message".into(),
                reason: smol_str::format_smolstr!("{}", error),
                raw_data: None,
            }
        })
    }

    pub fn decode<T: DeserializeOwned>(&self, payload: &[u8]) -> Result<T, MarketParticipantError> {
        serde_json::from_slice(payload)
            .map_err(|_error| MarketParticipantError::DecodeFailed {
                operation: "decode Binance message".into(),
                raw_data: Bytes::copy_from_slice(payload),
            })
            .map_err(|error| match error {
                MarketParticipantError::DecodeFailed { .. } => error,
                other => other,
            })
    }

    /// The candlestick intervals Binance publishes on `route`.
    ///
    /// The set is the venue's own kline set: the second candle exists on the
    /// spot market only, and Binance publishes no two-day candle anywhere.
    pub fn published_timeframes(route: &VenueRoute) -> Vec<TimeFrame> {
        BINANCE_INTERVALS
            .iter()
            .map(|(interval, _)| *interval)
            .filter(|interval| interval != &TimeFrame::Second1 || publishes_second(route))
            .collect()
    }

    /// Maps one Binance error envelope onto a typed market-participant failure.
    ///
    /// The codes Binance answers with are its own and its message is the whole
    /// explanation it gives, so both are passed through rather than paraphrased.
    pub(crate) fn map_error(
        &self,
        error: &BinanceErrorResponse,
        raw: Option<Bytes>,
    ) -> MarketParticipantError {
        match error.code {
            -1003 => MarketParticipantError::RateLimited {
                retry_after_ms: None,
                remaining_quota: None,
            },
            -1021 | -1022 => MarketParticipantError::ProtocolError {
                operation: "map Binance authentication error".into(),
                reason: format_smolstr!(
                    "Binance authentication response omitted account identity: {}",
                    error.msg
                ),
                raw_data: raw,
            },
            -2010 => MarketParticipantError::ProtocolError {
                operation: "map Binance insufficient-balance error".into(),
                reason: format_smolstr!(
                    "Binance insufficient-balance response omitted asset identity: {}",
                    error.msg
                ),
                raw_data: raw,
            },
            -2013 => MarketParticipantError::OrderError {
                kind: OrderErrorKind::OrderNotFound,
                raw_data: raw,
            },
            code => MarketParticipantError::Rejected {
                code: format_smolstr!("{}", code),
                message: error.msg.clone(),
                retryable: code <= -1000 && code > -1100,
                raw_response: raw,
            },
        }
    }

    /// Maps one failed Binance HTTP response onto a typed market-participant failure.
    ///
    /// Binance reports a refused request as its own `{code, msg}` envelope, so
    /// that envelope is what a user is shown; a failure without one keeps the
    /// venue's plain text instead.
    pub(crate) fn response_error(
        &self,
        route: &VenueRoute,
        endpoint: &str,
        status: reqwest::StatusCode,
        body: Bytes,
    ) -> MarketParticipantError {
        match self.decode::<BinanceErrorResponse>(&body) {
            Ok(error) => self.map_error(&error, Some(body)),
            Err(_) => http_response_error(
                crate::adapter::error::FailureTarget::Route(route),
                endpoint,
                status,
                body,
            ),
        }
    }

    /// The string Binance expects for `timeframe` on `route`.
    ///
    /// The set is Binance's own kline set: an interval this route does not
    /// publish is refused here rather than by the venue.
    pub fn encode_timeframe(
        &self,
        route: &VenueRoute,
        timeframe: TimeFrame,
    ) -> Result<&'static str, MarketParticipantError> {
        BINANCE_INTERVALS
            .iter()
            .find(|(interval, _)| *interval == timeframe)
            .map(|(_, spelling)| *spelling)
            .filter(|_| publishes_second(route) || timeframe != TimeFrame::Second1)
            .ok_or_else(|| MarketParticipantError::UnsupportedTimeFrame {
                route: route.clone(),
                timeframe: smol_str::SmolStr::new(timeframe.label()),
            })
    }
    /// The frame that opens one public stream subscription.
    ///
    /// Binance answers it with the same request id, which is what lets the
    /// connection resolve the acknowledgement of this exact subscription.
    pub fn encode_subscription(
        &self,
        streams: &[impl AsRef<str>],
        id: u64,
    ) -> super::types::BinanceSubscriptionArgs {
        super::types::BinanceSubscriptionArgs {
            method: "SUBSCRIBE".into(),
            params: streams
                .iter()
                .map(|stream| stream.as_ref().into())
                .collect(),
            id,
        }
    }
}

#[derive(serde::Deserialize)]
struct BinanceResponseId {
    id: Option<u64>,
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    use super::BinanceCodec;
    use crate::{
        error::MarketParticipantError, market_participants::model::TimeFrame, routing::SPOT_SCOPE,
    };

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new(scope).expect("the scope should be canonical"),
        )
    }

    #[test]
    fn extracts_top_level_response_id_only() {
        assert_eq!(
            BinanceCodec::extract_response_id(br#"{"id":42,"status":200,"result":{}}"#),
            Some(42)
        );
        assert_eq!(
            BinanceCodec::extract_response_id(br#"{"data":{"id":42}}"#),
            None
        );
    }

    /// The venue's own kline spelling is what goes on the wire, and its 1M and 1m
    /// intervals are different candles.
    #[test]
    fn the_model_intervals_are_encoded_in_the_venue_spelling() {
        let codec = BinanceCodec;
        let spot = route("spot");

        assert_eq!(
            codec
                .encode_timeframe(&spot, TimeFrame::Minute1)
                .expect("the minute candle is published"),
            "1m"
        );
        assert_eq!(
            codec
                .encode_timeframe(&spot, TimeFrame::Month1)
                .expect("the monthly candle is published"),
            "1M"
        );
        assert_eq!(
            codec
                .encode_timeframe(&spot, TimeFrame::Hour8)
                .expect("the eight-hour candle is published"),
            "8h"
        );
    }

    /// A stream name that is not a bare string is rejected by the venue, so the
    /// frame is asserted rather than derived.
    #[test]
    fn a_subscription_frame_carries_bare_stream_names() {
        let args = BinanceCodec.encode_subscription(&["btcusdt@depth@100ms"], 11);
        let wire = serde_json::to_string(&args).expect("the frame should encode");

        assert_eq!(
            wire,
            r#"{"method":"SUBSCRIBE","params":["btcusdt@depth@100ms"],"id":11}"#
        );
    }

    /// Binance answers a refused request with its own envelope: the code and the
    /// words a user reads are the venue's own.
    #[test]
    fn a_refusal_envelope_reaches_the_user_as_the_venue_wrote_it() {
        let error = BinanceCodec.response_error(
            &route(SPOT_SCOPE),
            "https://api.binance.com/api/v3/exchangeInfo",
            reqwest::StatusCode::BAD_REQUEST,
            Bytes::from_static(br#"{"code":-1121,"msg":"Invalid symbol."}"#),
        );

        assert_eq!(error.market_participant_code(), Some("-1121"));
        assert!(matches!(
            error,
            MarketParticipantError::Rejected { message, retryable: false, .. } if message == "Invalid symbol."
        ));
    }

    /// A rate-limited envelope keeps the venue's own classification of it.
    #[test]
    fn a_rate_limit_envelope_maps_to_the_rate_limited_failure() {
        let error = BinanceCodec.response_error(
            &route(SPOT_SCOPE),
            "https://api.binance.com/api/v3/exchangeInfo",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Bytes::from_static(br#"{"code":-1003,"msg":"Too many requests."}"#),
        );

        assert!(matches!(error, MarketParticipantError::RateLimited { .. }));
    }

    /// The error mapping keeps the raw response when a mapped code needs context
    /// the venue did not send.
    #[test]
    fn market_participant_error_mapping_preserves_raw_response_when_context_is_missing() {
        let error = BinanceCodec.map_error(
            &super::BinanceErrorResponse {
                code: -2010,
                msg: "Account has insufficient balance".into(),
            },
            Some(Bytes::from_static(br#"{"code":-2010}"#)),
        );

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError {
                operation,
                reason,
                raw_data: Some(raw_data),
            } if operation == "map Binance insufficient-balance error"
                && reason.contains("asset identity")
                && raw_data == Bytes::from_static(br#"{"code":-2010}"#)
        ));
    }

    /// A failure Binance did not describe is reported as the transport reported
    /// it: a server failure is the connection, anything else the status line.
    #[test]
    fn a_failure_without_a_binance_envelope_keeps_the_transport_answer() {
        let server = BinanceCodec.response_error(
            &route(SPOT_SCOPE),
            "https://api.binance.com/api/v3/exchangeInfo",
            reqwest::StatusCode::BAD_GATEWAY,
            Bytes::from_static(b"<html>gateway</html>"),
        );
        assert!(matches!(
            server,
            MarketParticipantError::ConnectionFailed { .. }
        ));

        let refused = BinanceCodec.response_error(
            &route(SPOT_SCOPE),
            "https://api.binance.com/api/v3/exchangeInfo",
            reqwest::StatusCode::FORBIDDEN,
            Bytes::from_static(b"blocked by gateway"),
        );
        assert!(matches!(
            refused,
            MarketParticipantError::Rejected { code, message, retryable: false, .. }
                if code == "403" && message == "blocked by gateway"
        ));
    }
}
