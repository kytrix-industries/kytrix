use bytes::Bytes;
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};
use smol_str::{SmolStr, format_smolstr};

use crate::error::MarketParticipantError;

const VENUE_MESSAGE_LIMIT: usize = 200;

#[derive(Clone, Copy)]
pub(crate) enum FailureTarget<'a> {
    Route(&'a VenueRoute),
    Venue(&'a VenueId),
}

impl FailureTarget<'_> {
    fn transport_failed(
        self,
        endpoint: SmolStr,
        source: network::error::NetworkError,
    ) -> MarketParticipantError {
        match self {
            Self::Route(route) => MarketParticipantError::ConnectionFailed {
                route: route.clone(),
                endpoint,
                source,
            },
            Self::Venue(venue) => MarketParticipantError::VenueRequestFailed {
                venue: *venue,
                endpoint,
                source,
            },
        }
    }
}

pub(crate) fn http_response_error(
    target: FailureTarget<'_>,
    endpoint: &str,
    status: reqwest::StatusCode,
    body: Bytes,
) -> MarketParticipantError {
    http_response_error_with_wait(target, endpoint, status, None, body)
}

pub(crate) fn http_response_error_with_wait(
    target: FailureTarget<'_>,
    endpoint: &str,
    status: reqwest::StatusCode,
    retry_after_ms: Option<u64>,
    body: Bytes,
) -> MarketParticipantError {
    if status.is_server_error() {
        return target.transport_failed(
            endpoint.into(),
            network::error::NetworkError::Io {
                message: format_smolstr!("HTTP status {status}"),
            },
        );
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return MarketParticipantError::RateLimited {
            retry_after_ms: retry_after_ms.or_else(|| retry_after_millis(&body)),
            remaining_quota: None,
        };
    }
    let message = venue_message(&body).unwrap_or_else(|| {
        format_smolstr!("HTTP {} response without a venue error", status.as_u16())
    });
    MarketParticipantError::Rejected {
        code: format_smolstr!("{}", status.as_u16()),
        message,
        retryable: false,
        raw_response: Some(body),
    }
}

fn retry_after_millis(body: &[u8]) -> Option<u64> {
    let seconds: u64 = std::str::from_utf8(body).ok()?.trim().parse().ok()?;
    Some(seconds.saturating_mul(1_000))
}

fn venue_message(body: &[u8]) -> Option<SmolStr> {
    let text = std::str::from_utf8(body).ok()?;
    let printable: String = text
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .collect();
    let trimmed = printable.trim();
    if trimmed.is_empty() {
        return None;
    }
    let truncated = match trimmed.char_indices().nth(VENUE_MESSAGE_LIMIT) {
        Some((offset, _)) => &trimmed[..offset],
        None => trimmed,
    };
    Some(SmolStr::new(truncated))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    use super::{FailureTarget, http_response_error};

    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new("spot").expect("the scope should be canonical"),
        )
    }

    #[test]
    fn a_server_failure_is_reported_as_a_connection_failure_of_the_target() {
        let error = http_response_error(
            FailureTarget::Route(&route()),
            "https://api.binance.com",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            Bytes::from_static(b"<html>maintenance</html>"),
        );

        assert!(error.is_retryable());
        assert!(error.is_network_error());
        assert!(matches!(
            error,
            crate::error::MarketParticipantError::ConnectionFailed { .. }
        ));
    }

    #[test]
    fn a_refusal_without_an_envelope_keeps_the_venue_text() {
        let error = http_response_error(
            FailureTarget::Route(&route()),
            "https://api.hyperliquid.xyz/info",
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            Bytes::from_static(b"Failed to deserialize the JSON body into the target type"),
        );

        assert!(!error.is_retryable());
        assert!(matches!(
            error,
            crate::error::MarketParticipantError::Rejected { code, message, retryable: false, .. }
                if code == "422"
                    && message == "Failed to deserialize the JSON body into the target type"
        ));
    }

    #[test]
    fn an_unreadable_refusal_falls_back_to_the_status_line() {
        for body in [
            Bytes::new(),
            Bytes::from_static(b"   "),
            Bytes::from_static(&[0xff, 0xfe]),
        ] {
            let error = http_response_error(
                FailureTarget::Route(&route()),
                "https://api.binance.com",
                reqwest::StatusCode::FORBIDDEN,
                body,
            );

            assert!(matches!(
                error,
                crate::error::MarketParticipantError::Rejected { code, message, .. }
                    if code == "403"
                        && message == "HTTP 403 response without a venue error"
            ));
        }
    }

    #[test]
    fn an_exhausted_allowance_is_a_rate_limit_rather_than_a_refusal() {
        let error = http_response_error(
            FailureTarget::Route(&route()),
            "https://api.hyperliquid.xyz/info",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Bytes::from_static(b"null"),
        );

        assert!(error.is_retryable());
        assert!(error.is_rate_limit());
        assert!(matches!(
            error,
            crate::error::MarketParticipantError::RateLimited { .. }
        ));

        let stated = http_response_error(
            FailureTarget::Route(&route()),
            "https://api.hyperliquid.xyz/info",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Bytes::from_static(b"3"),
        );
        assert_eq!(stated.suggested_retry_delay_ms(), Some(3_000));
    }

    #[test]
    fn a_stated_wait_is_what_the_rate_limit_carries() {
        let error = super::http_response_error_with_wait(
            FailureTarget::Route(&route()),
            "https://api.hyperliquid.xyz/info",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some(30_000),
            Bytes::from_static(b"null"),
        );
        assert_eq!(error.suggested_retry_delay_ms(), Some(30_000));
        assert!(error.is_rate_limit());

        let from_body = super::http_response_error_with_wait(
            FailureTarget::Route(&route()),
            "https://api.hyperliquid.xyz/info",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            None,
            Bytes::from_static(b"7"),
        );
        assert_eq!(from_body.suggested_retry_delay_ms(), Some(7_000));
    }

    #[test]
    fn a_long_refusal_is_truncated() {
        let error = http_response_error(
            FailureTarget::Route(&route()),
            "https://api.binance.com",
            reqwest::StatusCode::BAD_REQUEST,
            Bytes::from(vec![b'x'; 4_000]),
        );

        assert!(matches!(
            error,
            crate::error::MarketParticipantError::Rejected { message, .. }
                if message.chars().count() == 200
        ));
    }
}
