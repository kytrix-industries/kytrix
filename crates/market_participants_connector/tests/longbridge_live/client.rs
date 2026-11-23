use std::{env, io};

use app_config::network::{ProxyCredentials, ResolvedProxy};
use reqwest::Client;
use smol_str::SmolStr;
use url::Url;

use crate::{bounds::CONNECT_TIMEOUT, credentials::Credentials, http::measure_token};

pub const PROXY_ENV: &str = "KYTRIX_NETWORK_TEST_PROXY";
pub const DEFAULT_PROXY: &str = "socks5h://127.0.0.1:7890";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Direct,
    Proxy,
}

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

pub fn quote_endpoint() -> io::Result<Url> {
    let mut endpoint =
        Url::parse("wss://openapi-quote.longbridge.com/v2").map_err(io::Error::other)?;
    endpoint
        .query_pairs_mut()
        .extend_pairs([("version", "1"), ("codec", "1"), ("platform", "9")]);
    Ok(endpoint)
}

static TRANSPORT: tokio::sync::OnceCell<Transport> = tokio::sync::OnceCell::const_new();

fn candidate_transports() -> [Transport; 2] {
    [Transport::Proxy, Transport::Direct]
}

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

pub fn proxy_for(transport: Transport) -> ResolvedProxy {
    if transport == Transport::Proxy {
        configured_proxy().unwrap_or(ResolvedProxy::NoProxy)
    } else {
        ResolvedProxy::NoProxy
    }
}
