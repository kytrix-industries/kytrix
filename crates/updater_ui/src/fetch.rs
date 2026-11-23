use app_config::network::{
    Config, ConfigError, KytrixServiceCapability, ProxyRoute, ResolvedProxy, RouteTarget,
    ServiceTarget,
};
use network::{
    proxy::resolve,
    transport::{
        http::{HttpClientClass, read_response_body},
        hub::{TransportHub, TransportPermit},
    },
};
use reqwest::{Response, StatusCode, header::LOCATION};
use semver::Version;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    error::{MetadataError, RedirectRefusal, UpdateRequestError, UpdaterError},
    publish,
    release::{PlatformKey, ReleaseInfo, ReleaseManifest},
};

pub async fn fetch_release(
    transport: &TransportHub,
    config: &Config,
    runtime: &Handle,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RedirectDecision {
    Follow(Url),
    Final,
    Refused(RedirectRefusal),
}

pub(crate) fn redirect_decision(
    current: &Url,
    status: StatusCode,
    location: Option<&str>,
    followed: usize,
    limit: usize,
) -> RedirectDecision {
    if !status.is_redirection() || status == StatusCode::NOT_MODIFIED {
        return RedirectDecision::Final;
    }
    let Some(location) = location else {
        return RedirectDecision::Refused(RedirectRefusal::MissingLocation);
    };
    if followed >= limit {
        return RedirectDecision::Refused(RedirectRefusal::HopLimit);
    }
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

async fn resolve_route(
    config: &Config,
    target: &RouteTarget,
    destination: &Url,
    runtime: &Handle,
    cancellation: &CancellationToken,
) -> Result<ResolvedProxy, ConfigError> {
    let system = match target {
        RouteTarget::Service(ServiceTarget::KytrixService { capability }) => matches!(
            config.service_proxy_route_for(&ServiceTarget::KytrixService {
                capability: *capability
            }),
            ProxyRoute::System
        ),
        _ => false,
    };
    if !system {
        return resolve(config, target, destination);
    }

    let config = config.clone();
    let task_target = target.clone();
    let destination = destination.clone();
    let resolving = runtime.spawn_blocking(move || resolve(&config, &task_target, &destination));
    let refused = || ConfigError::InvalidSystemProxy {
        target: target.clone(),
    };
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(refused()),
        result = resolving => result.unwrap_or_else(|_| Err(refused())),
    }
}
async fn exchange(
    transport: &TransportHub,
    config: &Config,
    runtime: &Handle,
    capability: KytrixServiceCapability,
    class: HttpClientClass,
    mut url: Url,
    cancellation: &CancellationToken,
) -> Result<(TransportPermit, Response), UpdaterError> {
    let operation = transport.begin_operation().map_err(UpdaterError::Client)?;

    let target = RouteTarget::Service(ServiceTarget::KytrixService { capability });
    let limit = class.redirect_limit();
    let mut followed = 0;

    loop {
        let route = resolve_route(config, &target, &url, runtime, cancellation)
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
