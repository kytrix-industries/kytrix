//! The OKX v5 wire codec.
//!
//! OKX spells its own protocol values: the candlestick bars, the statistics
//! periods, the instrument types and the trade modes. Those spellings live here
//! and nowhere else, so the model's canonical interval label never reaches the
//! wire, and an interval this venue does not publish is refused with the route
//! that refused it.
//!
//! The codec also owns the venue's listing identity: an OKX listing is
//! continuous exactly while its `instId` and its instrument type are the same,
//! which is what [`OkxCodec::listing_key`] derives a `VenueListingKey` from.

use bytes::Bytes;
use market_participants_model::{
    identifier::{NativeSymbol, VenueScopeId},
    listing::{
        continuity::{ListingContinuityKey, ListingContinuityTerms},
        route::{VenueListingKey, VenueRoute},
        venue::OkxInstrumentType,
    },
    routing::CapabilityId,
};
use serde::{Serialize, de::DeserializeOwned};
use smol_str::{SmolStr, format_smolstr};

#[cfg(test)]
use super::types::OkxCancelOrderRequest;
use super::types::{OkxErrorEnvelope, OkxOrderRequest};
use crate::{
    account::shared::MarginMode,
    adapter::error::http_response_error,
    error::MarketParticipantError,
    market_participants::model::{StatisticsTimeFrame, TimeFrame},
    routing::{SPOT_SCOPE, SWAP_SCOPE},
};

/// The one wire codec of this adapter; it carries no state.
pub(crate) const CODEC: OkxCodec = OkxCodec;

/// The OKX codes whose failure is the venue being temporarily unable to serve a
/// request.
///
/// OKX's own wording is what decides membership: a matching-engine upgrade, an
/// endpoint timeout, an offline API, a busy system and a system error all tell
/// the caller to try again. The rest of its code space — an empty parameter, a
/// missing `instFamily`, an unknown instrument — refuses the request itself, and
/// reporting those as retryable would promise a retry that cannot succeed.
/// Rate limiting is absent because it is mapped to `RateLimited`, which carries
/// the venue's own retry guidance.
const TRANSIENT_ERROR_CODES: [&str; 5] = ["50001", "50004", "50005", "50013", "50026"];

#[derive(Clone, Copy, Debug, Default)]
pub struct OkxCodec;

impl OkxCodec {
    /// Extracts the correlation ID from an OKX WebSocket response.
    pub fn extract_response_id(payload: &[u8]) -> Option<u64> {
        serde_json::from_slice::<OkxResponseIdEnvelope>(payload)
            .ok()
            .and_then(|response| response.id)
            .and_then(|id| match id {
                OkxResponseId::Number(value) => Some(value),
                OkxResponseId::Text(value) => value.parse().ok(),
            })
    }

    pub fn encode<T: Serialize>(&self, value: &T) -> Result<Bytes, MarketParticipantError> {
        serde_json::to_vec(value).map(Bytes::from).map_err(|error| {
            MarketParticipantError::ProtocolError {
                operation: "encode OKX message".into(),
                reason: format_smolstr!("{error}"),
                raw_data: None,
            }
        })
    }

    pub fn decode<T: DeserializeOwned>(&self, payload: &[u8]) -> Result<T, MarketParticipantError> {
        serde_json::from_slice(payload).map_err(|_| MarketParticipantError::DecodeFailed {
            operation: "decode OKX message".into(),
            raw_data: Bytes::copy_from_slice(payload),
        })
    }

    /// The `instType` OKX selects instruments with on one scope.
    ///
    /// A scope this adapter does not serve has no instrument type at all: the
    /// caller answers `UnsupportedCapability` for it instead of guessing one.
    pub fn scope_instrument_type(&self, scope: &VenueScopeId) -> Option<OkxInstrumentType> {
        match scope.as_str() {
            SPOT_SCOPE => Some(OkxInstrumentType::Spot),
            SWAP_SCOPE => Some(OkxInstrumentType::Perpetual),
            _ => None,
        }
    }

    /// The wire value of one instrument type.
    pub fn instrument_type_param(&self, instrument_type: OkxInstrumentType) -> &'static str {
        match instrument_type {
            OkxInstrumentType::Spot => "SPOT",
            OkxInstrumentType::Perpetual => "SWAP",
            OkxInstrumentType::Future => "FUTURES",
            OkxInstrumentType::Option => "OPTION",
        }
    }

    /// The `bar` OKX expects for one candlestick interval.
    ///
    /// OKX capitalizes the unit letter above minutes and publishes 2-day and
    /// 3-day bars; it publishes neither a one-second nor an eight-hour bar on
    /// either of the scopes this adapter serves, and it publishes no
    /// two/ten/twenty/forty-five-minute bar, no three-hour bar and nothing above
    /// a month, so those are refused rather than sent as an interval the venue
    /// would reinterpret.
    pub fn encode_timeframe(
        &self,
        route: &VenueRoute,
        timeframe: TimeFrame,
    ) -> Result<&'static str, MarketParticipantError> {
        match timeframe {
            TimeFrame::Minute1 => Ok("1m"),
            TimeFrame::Minute3 => Ok("3m"),
            TimeFrame::Minute5 => Ok("5m"),
            TimeFrame::Minute15 => Ok("15m"),
            TimeFrame::Minute30 => Ok("30m"),
            TimeFrame::Hour1 => Ok("1H"),
            TimeFrame::Hour2 => Ok("2H"),
            TimeFrame::Hour4 => Ok("4H"),
            TimeFrame::Hour6 => Ok("6H"),
            TimeFrame::Hour12 => Ok("12H"),
            TimeFrame::Day1 => Ok("1D"),
            TimeFrame::Day2 => Ok("2D"),
            TimeFrame::Day3 => Ok("3D"),
            TimeFrame::Week1 => Ok("1W"),
            TimeFrame::Month1 => Ok("1M"),
            TimeFrame::Month3 => Ok("3M"),
            TimeFrame::Second1
            | TimeFrame::Minute2
            | TimeFrame::Minute10
            | TimeFrame::Minute20
            | TimeFrame::Minute45
            | TimeFrame::Hour3
            | TimeFrame::Hour8
            | TimeFrame::Quarter1
            | TimeFrame::Year1 => Err(MarketParticipantError::UnsupportedTimeFrame {
                route: route.clone(),
                timeframe: SmolStr::new_static(timeframe.label()),
            }),
        }
    }

    /// The `period` OKX expects for one statistics interval.
    pub fn encode_statistics_timeframe(&self, timeframe: StatisticsTimeFrame) -> &'static str {
        match timeframe {
            StatisticsTimeFrame::Minute5 => "5m",
            StatisticsTimeFrame::Minute15 => "15m",
            StatisticsTimeFrame::Minute30 => "30m",
            StatisticsTimeFrame::Hour1 => "1H",
            StatisticsTimeFrame::Hour2 => "2H",
            StatisticsTimeFrame::Hour4 => "4H",
            StatisticsTimeFrame::Hour6 => "6H",
            StatisticsTimeFrame::Hour12 => "12H",
            StatisticsTimeFrame::Day1 => "1D",
        }
    }

    /// The `tdMode` OKX expects for one order on one scope.
    ///
    /// A spot order is a cash order and may not carry a perpetual margin mode; a
    /// swap order always carries one, because OKX answers a swap order without a
    /// mode from the account's current default instead of the one that was meant.
    pub fn encode_trade_mode(
        &self,
        route: &VenueRoute,
        margin_mode: Option<MarginMode>,
    ) -> Result<&'static str, MarketParticipantError> {
        match (route.scope.as_str(), margin_mode) {
            (SPOT_SCOPE, None) => Ok("cash"),
            (SPOT_SCOPE, Some(_)) => Err(MarketParticipantError::Configuration(
                "a spot order cannot select a perpetual margin mode".into(),
            )),
            (SWAP_SCOPE, Some(MarginMode::Cross)) => Ok("cross"),
            (SWAP_SCOPE, Some(MarginMode::Isolated)) => Ok("isolated"),
            (SWAP_SCOPE, None) => Err(MarketParticipantError::Configuration(
                "a swap order requires an explicit margin mode".into(),
            )),
            _ => Err(MarketParticipantError::UnsupportedCapability {
                route: route.clone(),
                capability: CapabilityId::AuthenticatedTrading,
            }),
        }
    }

    pub fn encode_order_request(
        &self,
        inst_id: impl Into<SmolStr>,
        td_mode: impl Into<SmolStr>,
        side: impl Into<SmolStr>,
        ord_type: impl Into<SmolStr>,
        size: impl Into<SmolStr>,
        price: Option<impl Into<SmolStr>>,
    ) -> OkxOrderRequest {
        OkxOrderRequest {
            inst_id: inst_id.into(),
            td_mode: td_mode.into(),
            side: side.into(),
            ord_type: ord_type.into(),
            sz: size.into(),
            px: price.map(Into::into),
        }
    }

    #[cfg(test)]
    pub(crate) fn encode_cancel_request(
        &self,
        inst_id: impl Into<SmolStr>,
        order_id: impl Into<SmolStr>,
    ) -> OkxCancelOrderRequest {
        OkxCancelOrderRequest {
            inst_id: inst_id.into(),
            ord_id: order_id.into(),
        }
    }

    /// Derives the identity of one listing from the venue's own identifiers.
    ///
    /// The continuity terms are the `instId` and the instrument type, which are
    /// the two values that make one OKX listing the same listing across
    /// publications; a display alias never takes part in it.
    pub fn listing_key(
        &self,
        route: &VenueRoute,
        instrument_type: OkxInstrumentType,
        instrument_id: &str,
    ) -> Result<VenueListingKey, MarketParticipantError> {
        let instrument_id = NativeSymbol::new(instrument_id).map_err(|error| {
            MarketParticipantError::ProtocolError {
                operation: "derive OKX listing identity".into(),
                reason: format_smolstr!("{error}"),
                raw_data: None,
            }
        })?;
        ListingContinuityKey {
            route: route.clone(),
            terms: ListingContinuityTerms::Okx {
                instrument_id,
                instrument_type,
            },
        }
        .listing_key()
        .map_err(|error| MarketParticipantError::ProtocolError {
            operation: "derive OKX listing identity".into(),
            reason: format_smolstr!("{error}"),
            raw_data: None,
        })
    }

    /// Maps one OKX response code onto a typed market-participant failure.
    pub fn map_error(
        &self,
        code: &str,
        message: impl Into<SmolStr>,
        raw_data: Option<Bytes>,
    ) -> MarketParticipantError {
        let message = message.into();
        match code {
            "50011" => MarketParticipantError::RateLimited {
                retry_after_ms: None,
                remaining_quota: None,
            },
            "51020" => MarketParticipantError::ProtocolError {
                operation: "map OKX insufficient-balance error".into(),
                reason: format_smolstr!(
                    "OKX insufficient-balance response omitted asset identity: {message}"
                ),
                raw_data,
            },
            "50113" => MarketParticipantError::ProtocolError {
                operation: "map OKX authentication error".into(),
                reason: format_smolstr!(
                    "OKX authentication response omitted account identity: {message}"
                ),
                raw_data,
            },
            _ => MarketParticipantError::Rejected {
                code: code.into(),
                message,
                retryable: TRANSIENT_ERROR_CODES.contains(&code),
                raw_response: raw_data,
            },
        }
    }

    /// Maps one failed OKX HTTP response onto a typed market-participant failure.
    ///
    /// OKX reports a request it refused as its own `{code, msg}` envelope inside
    /// the response, so a user is shown the venue's own code and words; a failure
    /// that carries no envelope keeps the venue's plain text instead.
    pub fn response_error(
        &self,
        route: &VenueRoute,
        endpoint: &str,
        status: reqwest::StatusCode,
        body: Bytes,
    ) -> MarketParticipantError {
        match serde_json::from_slice::<OkxErrorEnvelope>(&body) {
            Ok(envelope) if envelope.code != "0" => self.map_error(
                envelope.code.as_str(),
                envelope.msg.unwrap_or_default(),
                Some(body),
            ),
            _ => http_response_error(
                crate::adapter::error::FailureTarget::Route(route),
                endpoint,
                status,
                body,
            ),
        }
    }
}

#[derive(serde::Deserialize)]
struct OkxResponseIdEnvelope {
    id: Option<OkxResponseId>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum OkxResponseId {
    Number(u64),
    Text(SmolStr),
}

#[cfg(test)]
mod public_api_tests {
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::{route::VenueRoute, venue::OkxInstrumentType},
    };

    use super::OkxCodec;
    use crate::{
        account::shared::MarginMode,
        market_participants::model::TimeFrame,
        routing::{SPOT_SCOPE, SWAP_SCOPE},
    };

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Okx,
            VenueScopeId::new(scope).expect("the scope id is canonical"),
        )
    }

    #[test]
    fn okx_order_codec_matches_v5_order_shape() {
        let codec = OkxCodec;
        let request = codec.encode_order_request(
            "BTC-USDT-SWAP",
            "cross",
            "buy",
            "limit",
            "1",
            Some("50000"),
        );
        let wire = serde_json::to_string(&request).unwrap();
        assert!(wire.contains("\"instId\":\"BTC-USDT-SWAP\""));
        assert!(wire.contains("\"tdMode\":\"cross\""));
        assert!(wire.contains("\"ordType\":\"limit\""));
        assert!(wire.contains("\"px\":\"50000\""));
    }

    #[test]
    fn okx_cancel_codec_matches_v5_cancel_shape() {
        let request = OkxCodec.encode_cancel_request("BTC-USDT", "12345");
        let wire = serde_json::to_string(&request).unwrap();
        assert_eq!(wire, r#"{"instId":"BTC-USDT","ordId":"12345"}"#);
    }

    #[test]
    fn okx_trade_mode_matches_scope_and_margin_settings() {
        let codec = OkxCodec;
        let spot = route(SPOT_SCOPE);
        let swap = route(SWAP_SCOPE);
        assert_eq!(codec.encode_trade_mode(&spot, None).unwrap(), "cash");
        assert_eq!(
            codec
                .encode_trade_mode(&swap, Some(MarginMode::Cross))
                .unwrap(),
            "cross"
        );
        assert_eq!(
            codec
                .encode_trade_mode(&swap, Some(MarginMode::Isolated))
                .unwrap(),
            "isolated"
        );
        assert!(codec.encode_trade_mode(&swap, None).is_err());
        assert!(
            codec
                .encode_trade_mode(&spot, Some(MarginMode::Cross))
                .is_err()
        );
    }

    /// The wire bars are OKX's own spellings, and the two intervals the venue
    /// does not publish are refused instead of being sent as another interval.
    #[test]
    fn okx_candle_bars_are_the_venues_own_spellings() {
        let codec = OkxCodec;
        let swap = route(SWAP_SCOPE);
        assert_eq!(
            codec.encode_timeframe(&swap, TimeFrame::Hour4).unwrap(),
            "4H"
        );
        assert_eq!(
            codec.encode_timeframe(&swap, TimeFrame::Day2).unwrap(),
            "2D"
        );
        assert_eq!(
            codec.encode_timeframe(&swap, TimeFrame::Month1).unwrap(),
            "1M"
        );
        assert!(matches!(
            codec.encode_timeframe(&swap, TimeFrame::Hour8),
            Err(crate::error::MarketParticipantError::UnsupportedTimeFrame { timeframe, .. })
                if timeframe == "8h"
        ));
    }

    /// One instrument type is what one scope addresses, and no other scope has one.
    #[test]
    fn okx_scopes_map_onto_their_own_instrument_types() {
        let codec = OkxCodec;
        let spot = VenueScopeId::new(SPOT_SCOPE).expect("the scope id is canonical");
        let swap = VenueScopeId::new(SWAP_SCOPE).expect("the scope id is canonical");
        let futures = VenueScopeId::new("futures").expect("the scope id is canonical");

        assert_eq!(
            codec.scope_instrument_type(&spot),
            Some(OkxInstrumentType::Spot)
        );
        assert_eq!(
            codec.scope_instrument_type(&swap),
            Some(OkxInstrumentType::Perpetual)
        );
        assert_eq!(codec.scope_instrument_type(&futures), None);
        assert_eq!(
            codec.instrument_type_param(OkxInstrumentType::Perpetual),
            "SWAP"
        );
    }

    /// The listing identity is the venue's own identity: two instruments of one
    /// route differ, and the same instrument on two scopes differs as well.
    #[test]
    fn okx_listing_identity_follows_the_instrument_id_and_the_scope() {
        let codec = OkxCodec;
        let swap = route(SWAP_SCOPE);
        let spot = route(SPOT_SCOPE);

        let first = codec
            .listing_key(&swap, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP")
            .expect("the instrument id is a stable identity");
        let same = codec
            .listing_key(&swap, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP")
            .expect("the instrument id is a stable identity");
        let other = codec
            .listing_key(&swap, OkxInstrumentType::Perpetual, "ETH-USDT-SWAP")
            .expect("the instrument id is a stable identity");
        let cross_scope = codec
            .listing_key(&spot, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP")
            .expect("the instrument id is a stable identity");

        assert_eq!(first, same);
        assert_ne!(first, other);
        assert_ne!(first, cross_scope);
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    use super::OkxCodec;
    use crate::{error::MarketParticipantError, routing::SWAP_SCOPE};

    /// The route one refused request is reported against.
    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Okx,
            VenueScopeId::new(SWAP_SCOPE).expect("swap is a canonical scope id"),
        )
    }

    #[test]
    fn extracts_top_level_response_id_from_number_or_string() {
        assert_eq!(
            OkxCodec::extract_response_id(br#"{"id":42,"code":"0","data":[]}"#),
            Some(42)
        );
        assert_eq!(
            OkxCodec::extract_response_id(br#"{"id":"43","code":"0","data":[]}"#),
            Some(43)
        );
        assert_eq!(
            OkxCodec::extract_response_id(br#"{"data":{"id":44}}"#),
            None
        );
    }

    #[test]
    fn authentication_error_without_account_context_is_protocol_error() {
        let error = OkxCodec.map_error(
            "50113",
            "Invalid API key",
            Some(Bytes::from_static(br#"{"code":"50113"}"#)),
        );

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError {
                operation,
                reason,
                raw_data: Some(raw_data),
            } if operation == "map OKX authentication error"
                && reason.contains("account identity")
                && raw_data == Bytes::from_static(br#"{"code":"50113"}"#)
        ));
    }

    #[test]
    fn insufficient_balance_error_without_asset_context_is_protocol_error() {
        let error = OkxCodec.map_error(
            "51020",
            "Insufficient balance",
            Some(Bytes::from_static(br#"{"code":"51020"}"#)),
        );

        assert!(matches!(
            error,
            MarketParticipantError::ProtocolError {
                operation,
                reason,
                raw_data: Some(raw_data),
            } if operation == "map OKX insufficient-balance error"
                && reason.contains("asset identity")
                && raw_data == Bytes::from_static(br#"{"code":"51020"}"#)
        ));
    }

    /// OKX answers a refused request with its own envelope inside the failure
    /// status: the code and the words a user reads are the venue's own.
    #[test]
    fn a_refusal_envelope_reaches_the_user_as_the_venue_wrote_it() {
        let error = OkxCodec.response_error(
            &route(),
            "https://openapi.okx.com",
            reqwest::StatusCode::BAD_REQUEST,
            Bytes::from_static(
                br#"{"code":"50015","data":[],"msg":"Either parameter instFamily or uly is required"}"#,
            ),
        );

        assert_eq!(error.market_participant_code(), Some("50015"));
        assert!(matches!(
            error,
            MarketParticipantError::Rejected { message, retryable: false, .. }
                if message == "Either parameter instFamily or uly is required"
        ));
    }

    /// Retryability is the venue's own classification: a system error says to
    /// try again, while a refusal of the request itself never does.
    #[test]
    fn only_the_transient_okx_codes_are_retryable() {
        for code in ["50001", "50004", "50005", "50013", "50026"] {
            let error = OkxCodec.response_error(
                &route(),
                "https://openapi.okx.com",
                reqwest::StatusCode::BAD_REQUEST,
                Bytes::from(format!(r#"{{"code":"{code}","msg":"try again later"}}"#)),
            );

            assert!(error.is_retryable(), "{code} is a transient failure");
        }

        for code in ["50014", "50015", "51000", "51001", "51004"] {
            let error = OkxCodec.response_error(
                &route(),
                "https://openapi.okx.com",
                reqwest::StatusCode::BAD_REQUEST,
                Bytes::from(format!(r#"{{"code":"{code}","msg":"refused"}}"#)),
            );

            assert!(!error.is_retryable(), "{code} is a permanent refusal");
        }
    }

    /// Rate limiting keeps its own mapping, which is what carries the venue's
    /// retry guidance.
    #[test]
    fn a_rate_limit_envelope_maps_to_the_rate_limited_failure() {
        let error = OkxCodec.response_error(
            &route(),
            "https://openapi.okx.com",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Bytes::from_static(br#"{"code":"50011","msg":"Request too frequent"}"#),
        );

        assert!(matches!(error, MarketParticipantError::RateLimited { .. }));
    }

    /// A failure OKX did not describe is reported as the transport reported it:
    /// a server failure is the connection, anything else keeps the status line.
    #[test]
    fn a_failure_without_an_okx_envelope_keeps_the_transport_answer() {
        let server = OkxCodec.response_error(
            &route(),
            "https://openapi.okx.com",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            Bytes::from_static(b"<html>maintenance</html>"),
        );
        assert!(matches!(
            server,
            MarketParticipantError::ConnectionFailed { .. }
        ));

        let refused = OkxCodec.response_error(
            &route(),
            "https://openapi.okx.com",
            reqwest::StatusCode::FORBIDDEN,
            Bytes::from_static(b"blocked by gateway"),
        );
        assert!(matches!(
            refused,
            MarketParticipantError::Rejected { code, message, retryable: false, .. }
                if code == "403" && message == "blocked by gateway"
        ));
    }

    /// A failure status that still claims success says nothing about a failure,
    /// so it is reported as the status line rather than as OKX code `0`.
    #[test]
    fn a_success_code_on_a_failure_status_is_reported_as_the_status_line() {
        let error = OkxCodec.response_error(
            &route(),
            "https://openapi.okx.com",
            reqwest::StatusCode::BAD_GATEWAY,
            Bytes::from_static(br#"{"code":"0","msg":""}"#),
        );

        assert!(matches!(
            error,
            MarketParticipantError::ConnectionFailed { .. }
        ));
    }
}
