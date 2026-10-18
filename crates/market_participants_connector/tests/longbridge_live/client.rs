//! The line every live request takes, and the HTTP client it takes it on.
//!
//! `tests/mainnet.rs` routes every live check through the proxy its environment names and
//! defaults that name to a local SOCKS5 line; this suite follows the same convention and
//! tries the proxy first, because the venue's hosts are not reachable from every network
//! without one — and it falls back to a direct dial, because the suite has to work with no
//! proxy configured as well. The answer is cached for the process, so every case of one
//! run takes the same route.

use std::{env, io};

use app_config::network::{ProxyCredentials, ResolvedProxy};
use reqwest::Client;
use smol_str::SmolStr;
use url::Url;

use crate::{bounds::CONNECT_TIMEOUT, credentials::Credentials, http::measure_token};

/// The environment variable that names the proxy a live run dials through.
pub const PROXY_ENV: &str = "KYTRIX_NETWORK_TEST_PROXY";
pub const DEFAULT_PROXY: &str = "socks5h://127.0.0.1:7890";

/// How one live run reaches the venue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// A direct dial: no proxy is configured, or the proxy did not answer.
    Direct,
    /// The run dials through the proxy its environment names.
    Proxy,
}

/// The proxy one live run dials through, when it dials through one.
fn configured_proxy() -> Option<ResolvedProxy> {
    let raw = env::var(PROXY_ENV).unwrap_or_else(|_| DEFAULT_PROXY.to_owned());
    let url = Url::parse(raw.trim()).ok()?;
    match url.scheme() {
        "socks5" | "socks5h" => {
            let host = url.host_str()?;
            let port = url.port()?;
            let credentials = match (url.username(), url.password()) {
                ("", None) => None,
                (username, password) if !username.is_empty() => Some(ProxyCredentials {
                    username: SmolStr::new(
                        percent_encoding::percent_decode_str(username)
                            .decode_utf8()
                            .ok()?,
                    ),
                    password: SmolStr::new(
                        percent_encoding::percent_decode_str(password.unwrap_or(""))
                            .decode_utf8()
                            .ok()?,
                    ),
                }),
                _ => None,
            };
            Some(ResolvedProxy::Socks5 {
                host: SmolStr::new(host),
                port,
                credentials,
                resolve_dns_at_remote: url.scheme() == "socks5h",
            })
        }
        "http" | "https" => Some(ResolvedProxy::Http {
            host: SmolStr::new(url.host_str()?),
            port: url.port()?,
            credentials: None,
            tls_to_proxy: url.scheme() == "https",
        }),
        _ => None,
    }
}

/// The HTTP client pool one dial builds its own client from.
///
/// This mirrors the shared transport's own client construction — same timeouts, the
/// same HTTP/1.1 pin the WebSocket upgrade needs, and the same explicit proxy
/// decision — so a case here dials what the adapters dial rather than a differently
/// configured client.
pub fn live_http_client(transport: Transport) -> io::Result<Client> {
    let mut builder = Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .https_only(true)
        .tcp_nodelay(true)
        .no_proxy();
    if transport == Transport::Proxy
        && let Some(proxy) = configured_proxy()
        && let Some(endpoint) = proxy.endpoint_url()
    {
        let proxy = reqwest::Proxy::all(endpoint).map_err(io::Error::other)?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(io::Error::other)
}

/// The endpoint a data centre's quote socket is dialled at (§4.1).
///
/// The path is part of the venue's published address rather than something a
/// request path appends, which is why the query is built on top of the address the
/// venue publishes: `/v2?version=1&codec=1&platform=9`.
pub fn quote_endpoint() -> io::Result<Url> {
    let mut endpoint =
        Url::parse("wss://openapi-quote.longbridge.com/v2").map_err(io::Error::other)?;
    endpoint
        .query_pairs_mut()
        .extend_pairs([("version", "1"), ("codec", "1"), ("platform", "9")]);
    Ok(endpoint)
}

/// The transport this process's live run takes.
static TRANSPORT: tokio::sync::OnceCell<Transport> = tokio::sync::OnceCell::const_new();

/// The two routes a live run may take, in the order they are tried.
fn candidate_transports() -> [Transport; 2] {
    [Transport::Proxy, Transport::Direct]
}

/// Returns the transport the run takes, probing the venue the first time.
pub async fn chosen_transport(credentials: &Credentials) -> Transport {
    *TRANSPORT
        .get_or_init(|| async {
            for candidate in candidate_transports() {
                let client = live_http_client(candidate).expect("the live client builds");
                match measure_token(&client, credentials, true).await {
                    Ok(_) => return candidate,
                    Err(error) => println!("  {candidate:?} did not answer: {error}"),
                }
            }
            println!(
                "  neither a direct dial nor {PROXY_ENV} answered; the cases below report their \
                 own transport failures"
            );
            Transport::Direct
        })
        .await
}

/// The proxy this run dials through, for the raw socket a case holds itself.
pub fn proxy_for(transport: Transport) -> ResolvedProxy {
    if transport == Transport::Proxy {
        configured_proxy().unwrap_or(ResolvedProxy::NoProxy)
    } else {
        ResolvedProxy::NoProxy
    }
}
