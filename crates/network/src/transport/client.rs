use std::time::Duration;

use app_config::network::ResolvedProxy;
use bytes::Bytes;
use reqwest::{Client, ClientBuilder, Proxy, header::HeaderMap, redirect::Policy};

use crate::transport::error::TransportError;

pub(super) struct ClientSpec {
    pub connect_timeout: Duration,
    pub request_timeout: Option<Duration>,
    pub user_agent: Option<&'static str>,
    pub default_headers: Option<HeaderMap>,
    pub tcp_nodelay: bool,
    pub http1_only: bool,
}

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
