//! The HTTP failure mapping every venue adapter agrees on.
//!
//! A venue that reports a refused request in its own envelope gets that envelope
//! mapped by its codec; what is left here is the response that carries no
//! envelope at all, where the venue's own words are still the best explanation a
//! user can be given.

use bytes::Bytes;
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};
use smol_str::{SmolStr, format_smolstr};

use crate::error::MarketParticipantError;

/// The number of characters of a venue's own words one error keeps.
const VENUE_MESSAGE_LIMIT: usize = 200;

/// The identity one failed HTTP response is reported against.
///
/// A route-scoped request names its route; the one venue-level request this crate
/// makes — the discovery conversation a load opens before anything is registered —
/// names the venue, because it addresses the venue itself rather than any scope of
/// it.
#[derive(Clone, Copy)]
pub(crate) enum FailureTarget<'a> {
    /// One route's request.
    Route(&'a VenueRoute),
    /// The venue-level discovery conversation.
    Venue(&'a VenueId),
}

impl FailureTarget<'_> {
    /// Reports one transport failure against this target.
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

/// Turns one failed HTTP response without a venue envelope into a typed error.
///
/// A server-side failure is a connection failure of the target the request was
/// validated against; a venue saying its own allowance is exhausted is a rate
/// limit, which is an answer about timing rather than about what was asked; and
/// anything else is the venue refusing the request, reported with whatever the
/// venue said about it. Such a refusal is never retryable on its own: a body this
/// adapter could not read is not evidence that a second attempt would be answered
/// differently.
///
/// The wait a rate limit carries is read from the body here, which is all this caller
/// knows: a venue that stated it in the response's own `Retry-After` header is read by
/// [`http_response_error_with_wait`], which the caller that holds the header uses.
pub(crate) fn http_response_error(
    target: FailureTarget<'_>,
    endpoint: &str,
    status: reqwest::StatusCode,
    body: Bytes,
) -> MarketParticipantError {
    http_response_error_with_wait(target, endpoint, status, None, body)
}

/// [`http_response_error`] with the wait the venue stated in its own header.
///
/// A rate limit is an answer about timing rather than about what was asked, so what the
/// venue said about the wait travels with it: without this a caller acting on the typed
/// error would wait a delay this adapter guessed instead of the one it was given. The
/// header is believed over the body, and the body is what a venue that stated the wait
/// on no header at all leaves.
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
    // 429 is the one refusal that says what was missing rather than what was wrong:
    // the request was not served, and the same request can be served later.
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

/// How long a body that carries only a delay asks the caller to wait.
///
/// The venue states the wait in a `Retry-After` header where it states one at all,
/// and a proxy in front of it may state the same thing in the body instead: a body
/// that is a number of seconds and nothing else is read as that delay.
fn retry_after_millis(body: &[u8]) -> Option<u64> {
    let seconds: u64 = std::str::from_utf8(body).ok()?.trim().parse().ok()?;
    Some(seconds.saturating_mul(1_000))
}

/// The venue's own words in a body that carries no envelope.
///
/// Hyperliquid answers a failed `info` request with plain text and no JSON at
/// all. A body that is not printable UTF-8 says nothing a user can act on, so it
/// is reported as absent rather than rendered as escapes.
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

    /// The route one refused request is reported against.
    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new("spot").expect("the scope should be canonical"),
        )
    }

    /// A server failure is the route's connection failing, not the venue
    /// rejecting the request: the caller's retry policy depends on the
    /// difference.
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

    /// A response that carries no envelope is still the venue's own answer, so
    /// its plain text is what the user is shown.
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

    /// A refusal this adapter cannot read is reported as the status line rather
    /// than as an empty message.
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

    /// A venue that answers with its own allowance exhausted has said nothing about
    /// what was asked, so the refusal is a rate limit that travels with whatever the
    /// venue stated about the wait — not a refusal a caller should give up on.
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

    /// A venue that stated its wait in the response's own header is believed rather than
    /// a delay this adapter guessed: a caller acting on the typed error waits what it was
    /// told to, and a venue that stated it nowhere but the body is still read from there.
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

    /// A venue that answers a failure with a whole document contributes its
    /// opening words, not an unbounded message.
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
