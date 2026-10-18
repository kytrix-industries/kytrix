//! The two requests the updater makes, and the redirect policy both of them follow.
//!
//! Neither request lets the client follow redirects: GitHub answers
//! `releases/latest/download/...` with a redirect to the release's own asset, and
//! the transport's client is built with `Policy::none()` precisely so that every
//! hop is resolved against the configured route rather than reused from the first
//! one. The hops are therefore followed here, one at a time, and the budget for
//! them is the operation class's own.

use app_config::network::{Config, KytrixServiceCapability, RouteTarget, ServiceTarget};
use bytes::Bytes;
use network::{
    proxy::resolve_async_cancellable,
    transport::{
        http::{HttpClientClass, read_response_body, read_response_body_with_progress},
        hub::{TransportHub, TransportPermit},
    },
};
use reqwest::{Response, StatusCode, header::LOCATION};
use semver::Version;
use tokio_util::sync::CancellationToken;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use crate::{
    error::{MetadataError, RedirectRefusal, UpdateRequestError, UpdaterError},
    publish,
    release::{PlatformKey, ReleaseInfo, ReleaseManifest},
};

/// Reads the published metadata from `endpoint` and reduces it to the update this
/// build would install.
///
/// The endpoint is the caller's, not this module's: it is a build-time fact of the
/// application rather than of how a request is made, and what a request is sent to
/// is what the tests that drive this path replace.
///
/// `Ok(None)` is the endpoint answering that there is nothing newer than the
/// running build, which is what most checks are.
pub async fn fetch_release(
    transport: &TransportHub,
    config: &Config,
    runtime: &Runtime,
    endpoint: &Url,
    cancellation: &CancellationToken,
) -> Result<Option<ReleaseInfo>, UpdaterError> {
    let (_operation, response) = exchange(
        transport,
        config,
        runtime,
        KytrixServiceCapability::UpdaterMetadata,
        HttpClientClass::UpdaterMetadata,
        endpoint.clone(),
        cancellation,
    )
    .await?;

    if response.status() == StatusCode::NO_CONTENT {
        return Ok(None);
    }

    let body = read_response_body(response, HttpClientClass::UpdaterMetadata, cancellation)
        .await
        .map_err(|source| UpdaterError::Transport(UpdateRequestError::Read(source)))?;
    let manifest = ReleaseManifest::parse(&body)?;
    let current = Version::parse(publish::CURRENT_VERSION)
        .map_err(|source| UpdaterError::Metadata(MetadataError::Version(source)))?;
    let key = PlatformKey::current().ok_or(UpdaterError::UnsupportedPlatform)?;

    manifest.select(key, &current)
}

/// Downloads the payload of `release`, reporting the accepted bytes as they arrive.
///
/// `on_progress` is called `(bytes read so far, exact payload size when the
/// response names one)` on the task that is reading, so it must not block: it is
/// what hands the numbers to the interface, not what draws them.
pub async fn download(
    transport: &TransportHub,
    config: &Config,
    runtime: &Runtime,
    release: &ReleaseInfo,
    cancellation: &CancellationToken,
    on_progress: impl Fn(u64, Option<u64>),
) -> Result<Bytes, UpdaterError> {
    let (_operation, response) = exchange(
        transport,
        config,
        runtime,
        KytrixServiceCapability::UpdaterDownload,
        HttpClientClass::UpdaterDownload,
        release.asset.url.clone(),
        cancellation,
    )
    .await?;

    read_response_body_with_progress(
        response,
        HttpClientClass::UpdaterDownload,
        cancellation,
        on_progress,
    )
    .await
    .map_err(|source| UpdaterError::Transport(UpdateRequestError::Read(source)))
}

/// What to do with the response to a request that follows its own redirects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RedirectDecision {
    /// Send the same request to this destination.
    Follow(Url),
    /// This response answers the request.
    Final,
    /// This response redirects somewhere this client will not go.
    Refused(RedirectRefusal),
}

/// Decides what one response means, from the request's own URL and the location it named.
///
/// Pure: the hop budget is the only state involved and it is handed in, which is
/// what lets every refusal be exercised without a server.
///
/// `followed` counts the redirects already followed for this request, so the
/// budget is spent once it reaches `limit`; a request that is over budget fails
/// rather than being sent to a destination nothing bounds.
pub(crate) fn redirect_decision(
    current: &Url,
    status: StatusCode,
    location: Option<&str>,
    followed: usize,
    limit: usize,
) -> RedirectDecision {
    // `304 Not Modified` is a redirection by status code alone and names no
    // destination; nothing here asks a conditional question, so it is a final answer.
    if !status.is_redirection() || status == StatusCode::NOT_MODIFIED {
        return RedirectDecision::Final;
    }
    let Some(location) = location else {
        return RedirectDecision::Refused(RedirectRefusal::MissingLocation);
    };
    if followed >= limit {
        return RedirectDecision::Refused(RedirectRefusal::HopLimit);
    }
    // A relative location is resolved against the URL that was asked for, not
    // against the origin: the release assets are siblings of the metadata.
    let Ok(next) = current.join(location) else {
        return RedirectDecision::Refused(RedirectRefusal::MalformedLocation);
    };
    if !matches!(next.scheme(), "http" | "https") {
        return RedirectDecision::Refused(RedirectRefusal::Scheme);
    }
    if current.scheme() == "https" && next.scheme() != "https" {
        return RedirectDecision::Refused(RedirectRefusal::Downgrade);
    }

    RedirectDecision::Follow(next)
}

/// Sends one request, following its redirects, and keeps the transport operation open.
///
/// The returned permit is the caller's to hold for as long as it is still reading
/// the response: it is what keeps a shutdown from closing the transport under a
/// transfer that has not finished, so it must not be dropped at the end of this
/// call.
async fn exchange(
    transport: &TransportHub,
    config: &Config,
    runtime: &Runtime,
    capability: KytrixServiceCapability,
    class: HttpClientClass,
    mut url: Url,
    cancellation: &CancellationToken,
) -> Result<(TransportPermit, Response), UpdaterError> {
    let operation = transport.begin_operation().map_err(UpdaterError::Client)?;

    // What a request is addressed through is the Kytrix service that serves it,
    // which is what its line is configured under — see the applications page.
    let target = RouteTarget::Service(ServiceTarget::KytrixService { capability });
    let limit = class.redirect_limit();
    let mut followed = 0;

    loop {
        // Every hop is resolved again: the destination changed, and a route may be
        // destination-dependent, so reusing the first hop's route would send the
        // rest of the chain down a line the user did not choose for it.
        let route =
            resolve_async_cancellable(config, &target, &url, cancellation.child_token(), runtime)
                .await
                .map_err(UpdaterError::Route)?;
        let client = transport
            .http()
            .client(&route, class)
            .map_err(UpdaterError::Client)?;

        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(UpdaterError::Transport(UpdateRequestError::Cancelled));
            }
            response = client.get(url.clone()).send() => response
                .map_err(|source| UpdaterError::Transport(UpdateRequestError::Send(source)))?,
        };

        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok());
        match redirect_decision(&url, response.status(), location, followed, limit) {
            RedirectDecision::Follow(next) => {
                followed += 1;
                url = next;
            }
            RedirectDecision::Final => {
                if !response.status().is_success() {
                    return Err(UpdaterError::Transport(UpdateRequestError::Status {
                        status: response.status(),
                    }));
                }
                return Ok((operation, response));
            }
            RedirectDecision::Refused(refusal) => {
                return Err(UpdaterError::Transport(UpdateRequestError::Redirect(
                    refusal,
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    #[test]
    fn a_redirect_should_be_followed_to_the_absolute_location_it_names() {
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/latest.json"),
                StatusCode::FOUND,
                Some("https://cdn.invalid/releases/1.0.0/latest.json"),
                0,
                10,
            ),
            RedirectDecision::Follow(url("https://cdn.invalid/releases/1.0.0/latest.json"))
        );
    }

    #[test]
    fn a_relative_location_should_be_resolved_against_the_request() {
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/kytrix/releases/latest/download/latest.json"),
                StatusCode::TEMPORARY_REDIRECT,
                Some("../download/v1.0.0/latest.json"),
                0,
                10,
            ),
            RedirectDecision::Follow(url(
                "https://example.invalid/kytrix/releases/latest/download/v1.0.0/latest.json"
            ))
        );
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/latest.json"),
                StatusCode::MOVED_PERMANENTLY,
                Some("/assets/latest.json"),
                3,
                10,
            ),
            RedirectDecision::Follow(url("https://example.invalid/assets/latest.json"))
        );
    }

    #[test]
    fn the_hop_budget_should_end_the_chain() {
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/a"),
                StatusCode::FOUND,
                Some("/b"),
                9,
                10,
            ),
            RedirectDecision::Follow(url("https://example.invalid/b")),
            "the last hop of the budget is still followed"
        );
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/a"),
                StatusCode::FOUND,
                Some("/b"),
                10,
                10,
            ),
            RedirectDecision::Refused(RedirectRefusal::HopLimit),
            "a chain longer than the budget fails rather than continuing"
        );
    }

    #[test]
    fn a_redirect_out_of_https_should_be_refused() {
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/latest.json"),
                StatusCode::FOUND,
                Some("http://cdn.invalid/latest.json"),
                0,
                10,
            ),
            RedirectDecision::Refused(RedirectRefusal::Downgrade),
            "an encrypted request is never answered by handing the payload to a plaintext one"
        );
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/latest.json"),
                StatusCode::FOUND,
                Some("https://cdn.invalid/latest.json"),
                0,
                10,
            ),
            RedirectDecision::Follow(url("https://cdn.invalid/latest.json")),
            "an https destination is what the budget is for"
        );
    }

    #[test]
    fn a_location_no_request_can_be_sent_to_should_be_refused() {
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/latest.json"),
                StatusCode::FOUND,
                Some("file:///etc/passwd"),
                0,
                10,
            ),
            RedirectDecision::Refused(RedirectRefusal::Scheme)
        );
        assert_eq!(
            redirect_decision(
                &url("https://example.invalid/latest.json"),
                StatusCode::FOUND,
                None,
                0,
                10,
            ),
            RedirectDecision::Refused(RedirectRefusal::MissingLocation)
        );
    }

    #[test]
    fn anything_that_is_not_a_redirect_should_be_a_final_answer() {
        for status in [
            StatusCode::OK,
            StatusCode::NO_CONTENT,
            StatusCode::NOT_FOUND,
            StatusCode::NOT_MODIFIED,
        ] {
            assert_eq!(
                redirect_decision(
                    &url("https://example.invalid/latest.json"),
                    status,
                    Some("https://cdn.invalid/latest.json"),
                    0,
                    10,
                ),
                RedirectDecision::Final,
                "{status} answers the request"
            );
        }
    }

    #[test]
    fn an_http_request_may_follow_an_http_redirect() {
        assert_eq!(
            redirect_decision(
                &url("http://127.0.0.1:8080/latest.json"),
                StatusCode::FOUND,
                Some("http://127.0.0.1:8080/release/latest.json"),
                0,
                10,
            ),
            RedirectDecision::Follow(url("http://127.0.0.1:8080/release/latest.json")),
            "a route that was already plaintext has nothing to downgrade"
        );
    }
}
