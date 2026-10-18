//! HTTP client construction for one resolved route.

use std::time::Duration;

use app_config::network::ResolvedProxy;
use bytes::Bytes;
use reqwest::{Client, ClientBuilder, Proxy, header::HeaderMap, redirect::Policy};

use crate::transport::error::TransportError;

/// Everything a client needs beyond its route and the transport policy.
pub(super) struct ClientSpec {
    pub connect_timeout: Duration,
    pub request_timeout: Option<Duration>,
    pub user_agent: Option<&'static str>,
    pub default_headers: Option<HeaderMap>,
    pub tcp_nodelay: bool,
    /// Whether this client must speak HTTP/1.1 and nothing else.
    ///
    /// The WebSocket handshake is an HTTP/1.1 exchange: RFC 6455 upgrades a
    /// connection with `Upgrade`/`Connection` headers, which HTTP/2 forbids, so a
    /// server reached over `h2` answers a reset stream instead of `101`.
    ///
    /// That makes the negotiated version a correctness concern rather than a
    /// preference, and it cannot be left to default: the version reqwest offers in
    /// ALPN follows *feature unification*, so any crate in the binary that enables
    /// `reqwest/http2` — `dicebear-core` does, through `jsonschema` — silently
    /// switches every client in the process to offering `h2`.
    pub http1_only: bool,
}

/// Builds a client bound to one route. A route that fails to materialize is an
/// error, never a silent direct connection.
pub(super) fn build(
    route: &ResolvedProxy,
    spec: ClientSpec,
    https_only: bool,
    extra_root_certificates: &[Bytes],
) -> Result<Client, TransportError> {
    let mut builder = Client::builder()
        .connect_timeout(spec.connect_timeout)
        .https_only(https_only)
        .tcp_nodelay(spec.tcp_nodelay)
        // Redirect-capable callers resolve and send every hop explicitly so a
        // destination-dependent System/PAC route cannot be reused accidentally.
        .redirect(Policy::none())
        .retry(reqwest::retry::never());
    if spec.http1_only {
        builder = builder.http1_only();
    }
    if let Some(timeout) = spec.request_timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(user_agent) = spec.user_agent {
        builder = builder.user_agent(user_agent);
    }
    if let Some(headers) = spec.default_headers {
        builder = builder.default_headers(headers);
    }
    builder = install_route(builder, route)?;
    for certificate_der in extra_root_certificates {
        let certificate = reqwest::Certificate::from_der(certificate_der)
            .map_err(|_| TransportError::HttpClientConfiguration)?;
        builder = builder.add_root_certificate(certificate);
    }
    builder
        .build()
        .map_err(|_| TransportError::HttpClientConfiguration)
}

/// Installs the hop, or explicitly disables every proxy for a direct route.
///
/// Requesting the proxy first would leave reqwest's own proxy discovery in
/// charge whenever the route is direct.
fn install_route(
    builder: ClientBuilder,
    route: &ResolvedProxy,
) -> Result<ClientBuilder, TransportError> {
    let Some(endpoint) = route.endpoint_url() else {
        return Ok(builder.no_proxy());
    };
    let proxy = Proxy::all(endpoint).map_err(|_| TransportError::ProxyConfiguration)?;
    Ok(builder.no_proxy().proxy(proxy))
}
