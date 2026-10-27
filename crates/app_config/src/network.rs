//! The persisted network, proxy, and outbound-route configuration model.
//!
//! Every outbound request is routed by a [`ProxyRoute`]: a venue by its own line,
//! a Kytrix service by its application, and one that names no line by the
//! document's default line. The model is persisted with the rest of the
//! application settings and materialized into [`ResolvedProxy`] hops by the
//! `network` transport before any request is sent.
//!
//! A venue's own line lives in its market-participant settings rather than here: this module
//! is the proxy catalog and the routes over it, and the market-participant settings name one
//! of these lines for the venue they configure.

use std::{collections::BTreeMap, fmt, net::Ipv6Addr};

use market_participants_model::identifier::VenueId;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;
use url::{Host, Url};

/// Kytrix service capabilities that use independent outbound routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum KytrixServiceCapability {
    #[serde(rename = "account")]
    Account,
    #[serde(rename = "cryptocurrency_etf")]
    CryptocurrencyEtf,
    #[serde(rename = "news")]
    News,
    #[serde(rename = "financial_indices")]
    FinancialIndices,
    #[serde(rename = "x_account_monitoring")]
    XAccountMonitoring,
    #[serde(rename = "cryptocurrency_wallet_monitoring")]
    CryptocurrencyWalletMonitoring,
    #[serde(rename = "x_account_monitoring_media")]
    XAccountMonitoringMedia,
    /// The CMC crypto fear and greed index, a snapshot the Kytrix service reads.
    #[serde(rename = "cmc")]
    Cmc,
    /// The financial calendar, a snapshot of scheduled economic events.
    #[serde(rename = "financial_calendar")]
    FinancialCalendar,
    /// The Pentagon Pizza Index, a snapshot the Kytrix service reads.
    ///
    /// It is one of Kytrix's own outbound routes rather than a venue's: no market
    /// participant publishes it, and it has no catalog of its own. The index itself
    /// needs no account, but the service serves it over the authenticated connection
    /// the pane already holds, so it is configured here like every other capability
    /// and shares the cloud line with them.
    #[serde(rename = "pentagon_pizza_index")]
    PentagonPizzaIndex,
    #[serde(rename = "updater_metadata")]
    UpdaterMetadata,
    #[serde(rename = "updater_download")]
    UpdaterDownload,
    /// The strategic maritime chokepoints vessel feed the Kytrix service pushes for one area.
    #[serde(rename = "strategic_maritime_chokepoints_tracking")]
    StrategicMaritimeChokepointsTracking,
}

/// Generic outbound route identity.
///
/// The network crate never interprets a target; it forwards it as a typed
/// configuration key. A venue is not a target: its requests are routed by the
/// venue's own configuration, which names a line of its own or follows the
/// default line, so the only service a target still addresses is one of Kytrix's
/// own.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ServiceTarget {
    #[serde(rename = "kytrix_service")]
    KytrixService {
        #[serde(rename = "capability")]
        capability: KytrixServiceCapability,
    },
}

impl fmt::Display for ServiceTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KytrixService { capability } => {
                write!(formatter, "Kytrix service {capability:?}")
            }
        }
    }
}

/// Configured outbound choice for one target.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ProxyRoute {
    #[serde(rename = "direct")]
    Direct,
    #[serde(rename = "system")]
    System,
    #[serde(rename = "named")]
    Named { name: SmolStr },
}

impl ProxyRoute {
    pub fn named(name: impl Into<SmolStr>) -> Self {
        Self::Named { name: name.into() }
    }

    pub fn named_name(&self) -> Option<&str> {
        match self {
            Self::Named { name } => Some(name.as_str()),
            Self::Direct | Self::System => None,
        }
    }

    pub fn rename_catalog_proxy(&mut self, old_name: &str, new_name: &SmolStr) {
        if let Self::Named { name } = self
            && name == old_name
        {
            *name = new_name.clone();
        }
    }
}

/// Proxy catalog entry.
///
/// Named proxies are always SOCKS5 endpoints. `addr` accepts a host name, an
/// IPv4 literal, or an IPv6 literal in either bracketed or bare form.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub addr: SmolStr,
    pub port: u16,
    pub authorization: ProxyAuthorizationConfig,
    pub resolve_dns_at_remote: bool,
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyConfig")
            .field("addr", &self.addr)
            .field("port", &self.port)
            .field("authorization", &self.authorization)
            .field("resolve_dns_at_remote", &self.resolve_dns_at_remote)
            .finish()
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            addr: SmolStr::new_static(""),
            port: 0,
            authorization: ProxyAuthorizationConfig::None,
            resolve_dns_at_remote: true,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyAuthorizationConfig {
    None,
    UsernameAndPassword {
        username: SmolStr,
        password: SmolStr,
    },
}

impl fmt::Debug for ProxyAuthorizationConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::UsernameAndPassword { .. } => formatter
                .debug_struct("UsernameAndPassword")
                .field("username", &"<redacted>")
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ProxyCredentials {
    pub username: SmolStr,
    pub password: SmolStr,
}

impl fmt::Debug for ProxyCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyCredentials")
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// One outbound hop that a route has been materialized into.
///
/// `host` is a URL authority host: a host name, an IPv4 literal, or a bracketed
/// IPv6 literal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResolvedProxy {
    NoProxy,
    Socks5 {
        host: SmolStr,
        port: u16,
        credentials: Option<ProxyCredentials>,
        resolve_dns_at_remote: bool,
    },
    Http {
        host: SmolStr,
        port: u16,
        credentials: Option<ProxyCredentials>,
        tls_to_proxy: bool,
    },
}

impl ResolvedProxy {
    /// Renders the hop as the proxy URL an HTTP client expects.
    ///
    /// Credentials are carried in the URL userinfo, which is what both the
    /// SOCKS5 and the HTTP proxy paths of the HTTP client read them from.
    pub fn endpoint_url(&self) -> Option<String> {
        let (scheme, host, port, credentials) = match self {
            Self::NoProxy => return None,
            Self::Socks5 {
                host,
                port,
                credentials,
                resolve_dns_at_remote,
            } => (
                if *resolve_dns_at_remote {
                    "socks5h"
                } else {
                    "socks5"
                },
                host,
                *port,
                credentials.as_ref(),
            ),
            Self::Http {
                host,
                port,
                credentials,
                tls_to_proxy,
            } => (
                if *tls_to_proxy { "https" } else { "http" },
                host,
                *port,
                credentials.as_ref(),
            ),
        };
        let mut url = Url::parse(&format!("{scheme}://placeholder")).ok()?;
        url.set_host(Some(host)).ok()?;
        url.set_port(Some(port)).ok()?;
        if let Some(credentials) = credentials {
            url.set_username(&credentials.username).ok()?;
            url.set_password(Some(&credentials.password)).ok()?;
        }
        Some(url.to_string())
    }
}

/// Who one outbound route is resolved for.
///
/// A venue is routed by the venue's own configuration — the line it names, or the
/// default line it names none of — and a Kytrix service by its application, and
/// both are resolved through the same network layer, so a resolution that has to
/// name its subject names one of these. The two are not interchangeable — a venue
/// is not a service target, and the configuration that routes it is not the map a
/// service is looked up in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteTarget {
    /// One venue's requests.
    Venue(VenueId),
    /// One of Kytrix's own services.
    Service(ServiceTarget),
}

impl fmt::Display for RouteTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Venue(venue) => write!(formatter, "venue {venue}"),
            Self::Service(target) => write!(formatter, "{target}"),
        }
    }
}

impl From<&VenueId> for RouteTarget {
    fn from(venue: &VenueId) -> Self {
        Self::Venue(*venue)
    }
}

impl From<&ServiceTarget> for RouteTarget {
    fn from(target: &ServiceTarget) -> Self {
        Self::Service(target.clone())
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("proxy {name:?} selected for {target} does not exist")]
    DanglingProxyReference { target: RouteTarget, name: SmolStr },
    #[error("proxy {name:?} selected for {target} has an invalid address")]
    InvalidProxyAddress { target: RouteTarget, name: SmolStr },
    #[error("proxy {name:?} selected for {target} has an invalid port")]
    InvalidProxyPort { target: RouteTarget, name: SmolStr },
    #[error("proxy {name:?} selected for {target} has incomplete authentication")]
    IncompleteProxyCredentials { target: RouteTarget, name: SmolStr },
    #[error("system proxy selected for {target} is invalid")]
    InvalidSystemProxy { target: RouteTarget },
}

/// Why a named catalog entry cannot be materialized.
///
/// The catalog entry is the same for every target that selects it, so this
/// error deliberately carries no target: the default route has none, and two
/// targets sharing a proxy must not disagree about whether it is usable.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum NamedProxyError {
    #[error("proxy {name:?} does not exist")]
    Missing { name: SmolStr },
    #[error("proxy {name:?} has an invalid address")]
    InvalidAddress { name: SmolStr },
    #[error("proxy {name:?} has an invalid port")]
    InvalidPort { name: SmolStr },
    #[error("proxy {name:?} has incomplete authentication")]
    IncompleteCredentials { name: SmolStr },
}

impl NamedProxyError {
    /// Attributes this failure to the target that selected the entry.
    pub fn for_target(self, target: &RouteTarget) -> ConfigError {
        match self {
            Self::Missing { name } => ConfigError::DanglingProxyReference {
                target: target.clone(),
                name,
            },
            Self::InvalidAddress { name } => ConfigError::InvalidProxyAddress {
                target: target.clone(),
                name,
            },
            Self::InvalidPort { name } => ConfigError::InvalidProxyPort {
                target: target.clone(),
                name,
            },
            Self::IncompleteCredentials { name } => ConfigError::IncompleteProxyCredentials {
                target: target.clone(),
                name,
            },
        }
    }
}

/// Monotonic version of the proxy routing decision for one target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NetworkRouteGeneration(u64);

impl NetworkRouteGeneration {
    /// Creates a generation from a published counter value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the published counter value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The generations stamped onto one persisted [`Config`].
///
/// Generations advance only when the effective route for a target changes, so
/// a settings rewrite that leaves routing untouched keeps every consumer's
/// resolved connections valid. A venue and a Kytrix service are stamped apart:
/// they are routed by different configuration, and one moving must not
/// invalidate the other's connections.
#[derive(Clone, Debug, Default)]
pub struct NetworkRuntimeGenerations {
    pub default_route: NetworkRouteGeneration,
    /// The generation of each venue's proxy decision, keyed by venue.
    pub venues: BTreeMap<VenueId, NetworkRouteGeneration>,
    /// The generation of each Kytrix service's route, keyed by service target.
    pub services: BTreeMap<ServiceTarget, NetworkRouteGeneration>,
}

/// Immutable network-only configuration snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub proxies: BTreeMap<SmolStr, ProxyConfig>,
    pub proxy_default: ProxyRoute,
    /// The outbound route of Kytrix's own services, keyed by service target.
    pub proxy_applications: BTreeMap<ServiceTarget, ProxyRoute>,
    /// The outbound route of each venue that names one of its own, keyed by
    /// venue.
    ///
    /// A venue this map does not name follows [`Self::proxy_default`], which is
    /// what a venue whose configuration carries no line of its own is projected
    /// to.
    pub venue_routes: BTreeMap<VenueId, ProxyRoute>,
    pub default_route_generation: NetworkRouteGeneration,
    /// The generation of each venue's proxy decision, keyed by venue.
    pub route_generations: BTreeMap<VenueId, NetworkRouteGeneration>,
    /// The generation of each Kytrix service's route, keyed by service target.
    pub service_route_generations: BTreeMap<ServiceTarget, NetworkRouteGeneration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxies: BTreeMap::new(),
            proxy_default: ProxyRoute::Direct,
            proxy_applications: BTreeMap::new(),
            venue_routes: BTreeMap::new(),
            default_route_generation: NetworkRouteGeneration::default(),
            route_generations: BTreeMap::new(),
            service_route_generations: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Returns the outbound route one venue's requests take.
    ///
    /// A venue that names a line carries it; a venue that names none follows the
    /// default, exactly as a service without an override does.
    pub fn proxy_route_for(&self, venue: &VenueId) -> ProxyRoute {
        self.venue_routes
            .get(venue)
            .cloned()
            .unwrap_or_else(|| self.proxy_default.clone())
    }

    /// Returns the outbound route one Kytrix service takes.
    pub fn service_proxy_route_for(&self, target: &ServiceTarget) -> ProxyRoute {
        self.proxy_applications
            .get(target)
            .cloned()
            .unwrap_or_else(|| self.proxy_default.clone())
    }

    /// Returns the proxy generation of one venue.
    pub fn network_route_generation(&self, venue: &VenueId) -> NetworkRouteGeneration {
        self.route_generations
            .get(venue)
            .copied()
            .unwrap_or(self.default_route_generation)
    }

    /// Returns the proxy generation of one Kytrix service.
    pub fn service_network_route_generation(
        &self,
        target: &ServiceTarget,
    ) -> NetworkRouteGeneration {
        self.service_route_generations
            .get(target)
            .copied()
            .unwrap_or(self.default_route_generation)
    }

    /// Materializes a named SOCKS5 catalog entry for one target.
    ///
    /// Invalid bindings fail closed: a missing, malformed, or incomplete entry
    /// never degrades into a direct connection.
    pub fn resolve_named_proxy(
        &self,
        target: &RouteTarget,
        name: &str,
    ) -> Result<ResolvedProxy, ConfigError> {
        self.resolve_named_proxy_entry(name)
            .map_err(|error| error.for_target(target))
    }

    /// Materializes a named SOCKS5 catalog entry without a target context.
    ///
    /// The entry itself is independent of who selects it, which is what lets the
    /// configuration manager fingerprint the default route — the one route
    /// without a target.
    pub fn resolve_named_proxy_entry(&self, name: &str) -> Result<ResolvedProxy, NamedProxyError> {
        let proxy = self
            .proxies
            .get(name)
            .ok_or_else(|| NamedProxyError::Missing {
                name: SmolStr::new(name),
            })?;
        let host =
            normalize_proxy_host(&proxy.addr).ok_or_else(|| NamedProxyError::InvalidAddress {
                name: SmolStr::new(name),
            })?;
        if proxy.port == 0 {
            return Err(NamedProxyError::InvalidPort {
                name: SmolStr::new(name),
            });
        }
        let credentials = match &proxy.authorization {
            ProxyAuthorizationConfig::None => None,
            ProxyAuthorizationConfig::UsernameAndPassword { username, password } => {
                if username.trim().is_empty() || password.trim().is_empty() {
                    return Err(NamedProxyError::IncompleteCredentials {
                        name: SmolStr::new(name),
                    });
                }
                Some(ProxyCredentials {
                    username: username.clone(),
                    password: password.clone(),
                })
            }
        };
        Ok(ResolvedProxy::Socks5 {
            host,
            port: proxy.port,
            credentials,
            resolve_dns_at_remote: proxy.resolve_dns_at_remote,
        })
    }
}

/// Normalizes a configured or discovered proxy address into a URL authority
/// host: a host name, an IPv4 literal, or a bracketed IPv6 literal.
///
/// A bare IPv6 literal is bracketed so that every later endpoint rendering
/// stays valid URL syntax. This is the single place where both the named-proxy
/// catalog and the operating-system proxy readers turn an address into a host.
pub fn normalize_proxy_host(addr: &str) -> Option<SmolStr> {
    let addr = addr.trim();
    if addr.is_empty() {
        return None;
    }
    if let Ok(address) = addr.parse::<Ipv6Addr>() {
        return Some(format_smolstr!("[{address}]"));
    }
    match Host::parse(addr).ok()? {
        Host::Ipv6(address) => Some(format_smolstr!("[{address}]")),
        host => Some(SmolStr::new(host.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use market_participants_model::identifier::VenueId;
    use serde_json::json;
    use smol_str::SmolStr;
    use url::Url;

    use super::{
        Config, ConfigError, KytrixServiceCapability, NamedProxyError, NetworkRouteGeneration,
        ProxyAuthorizationConfig, ProxyConfig, ProxyCredentials, ProxyRoute, ResolvedProxy,
        RouteTarget, ServiceTarget,
    };

    /// The venue the proxy tests route for.
    fn venue() -> VenueId {
        VenueId::Binance
    }

    fn target() -> ServiceTarget {
        ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::XAccountMonitoringMedia,
        }
    }

    fn named_config(addr: &str, port: u16) -> Config {
        let mut config = Config::default();
        config.proxies.insert(
            SmolStr::new_static("hk"),
            ProxyConfig {
                addr: SmolStr::new(addr),
                port,
                authorization: ProxyAuthorizationConfig::None,
                resolve_dns_at_remote: true,
            },
        );
        config
    }

    #[test]
    fn named_proxy_accepts_host_names_and_both_ip_families() {
        let target = target();
        assert_eq!(
            named_config("proxy.example", 1080)
                .resolve_named_proxy(&RouteTarget::Service(target.clone()), "hk"),
            Ok(ResolvedProxy::Socks5 {
                host: SmolStr::new_static("proxy.example"),
                port: 1080,
                credentials: None,
                resolve_dns_at_remote: true,
            })
        );
        assert_eq!(
            named_config("127.0.0.1", 1080)
                .resolve_named_proxy(&RouteTarget::Service(target.clone()), "hk"),
            Ok(ResolvedProxy::Socks5 {
                host: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                credentials: None,
                resolve_dns_at_remote: true,
            })
        );
        for addr in ["::1", "[::1]"] {
            assert_eq!(
                named_config(addr, 1080)
                    .resolve_named_proxy(&RouteTarget::Service(target.clone()), "hk"),
                Ok(ResolvedProxy::Socks5 {
                    host: SmolStr::new_static("[::1]"),
                    port: 1080,
                    credentials: None,
                    resolve_dns_at_remote: true,
                }),
                "{addr}"
            );
        }
    }

    #[test]
    fn named_proxy_rejects_malformed_bindings() {
        let target = target();
        assert_eq!(
            Config::default().resolve_named_proxy(&RouteTarget::Service(target.clone()), "missing"),
            Err(ConfigError::DanglingProxyReference {
                target: RouteTarget::Service(target.clone()),
                name: SmolStr::new_static("missing"),
            })
        );
        for addr in [
            "",
            "   ",
            "bad host",
            "user@proxy.example",
            "proxy.example/path",
        ] {
            assert_eq!(
                named_config(addr, 1080)
                    .resolve_named_proxy(&RouteTarget::Service(target.clone()), "hk"),
                Err(ConfigError::InvalidProxyAddress {
                    target: RouteTarget::Service(target.clone()),
                    name: SmolStr::new_static("hk"),
                }),
                "{addr:?}"
            );
        }
        assert_eq!(
            named_config("proxy.example", 0)
                .resolve_named_proxy(&RouteTarget::Service(target.clone()), "hk"),
            Err(ConfigError::InvalidProxyPort {
                target: RouteTarget::Service(target),
                name: SmolStr::new_static("hk"),
            })
        );
    }

    #[test]
    fn named_proxy_requires_complete_credentials() {
        let target = target();
        let mut config = named_config("proxy.example", 1080);
        config.proxies.insert(
            SmolStr::new_static("hk"),
            ProxyConfig {
                addr: SmolStr::new_static("proxy.example"),
                port: 1080,
                authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                    username: SmolStr::new_static("user"),
                    password: SmolStr::new_static("  "),
                },
                resolve_dns_at_remote: false,
            },
        );
        assert_eq!(
            config.resolve_named_proxy(&RouteTarget::Service(target.clone()), "hk"),
            Err(ConfigError::IncompleteProxyCredentials {
                target: RouteTarget::Service(target.clone()),
                name: SmolStr::new_static("hk"),
            })
        );

        config.proxies.insert(
            SmolStr::new_static("hk"),
            ProxyConfig {
                addr: SmolStr::new_static("proxy.example"),
                port: 1080,
                authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                    username: SmolStr::new_static("user"),
                    password: SmolStr::new_static("pass word"),
                },
                resolve_dns_at_remote: false,
            },
        );
        assert_eq!(
            config.resolve_named_proxy(&RouteTarget::Service(target), "hk"),
            Ok(ResolvedProxy::Socks5 {
                host: SmolStr::new_static("proxy.example"),
                port: 1080,
                credentials: Some(ProxyCredentials {
                    username: SmolStr::new_static("user"),
                    password: SmolStr::new_static("pass word"),
                }),
                resolve_dns_at_remote: false,
            })
        );
    }

    #[test]
    fn an_entry_failure_should_not_depend_on_the_target_that_selected_it() {
        let mut config = named_config("proxy.example", 0);

        assert_eq!(
            config.resolve_named_proxy_entry("hk"),
            Err(NamedProxyError::InvalidPort {
                name: SmolStr::new_static("hk"),
            })
        );
        assert_eq!(
            config.resolve_named_proxy_entry("missing"),
            Err(NamedProxyError::Missing {
                name: SmolStr::new_static("missing"),
            })
        );
        assert_eq!(
            NamedProxyError::InvalidPort {
                name: SmolStr::new_static("hk"),
            }
            .for_target(&RouteTarget::Service(target())),
            ConfigError::InvalidProxyPort {
                target: RouteTarget::Service(target()),
                name: SmolStr::new_static("hk"),
            }
        );

        config.proxies.clear();
        assert_eq!(
            config.resolve_named_proxy(&RouteTarget::Service(target()), "hk"),
            Err(ConfigError::DanglingProxyReference {
                target: RouteTarget::Service(target()),
                name: SmolStr::new_static("hk"),
            })
        );
    }

    #[test]
    fn socks5_endpoints_carry_credentials_and_the_resolution_mode() {
        let route = ResolvedProxy::Socks5 {
            host: SmolStr::new_static("proxy.example"),
            port: 1080,
            credentials: Some(ProxyCredentials {
                username: SmolStr::new_static("user name"),
                password: SmolStr::new_static("p@ss:word"),
            }),
            resolve_dns_at_remote: true,
        };
        // The HTTP client percent-decodes the userinfo, so reserved characters
        // have to survive the round trip through the rendered URL.
        let url = Url::parse(&route.endpoint_url().expect("an endpoint")).unwrap();
        assert_eq!(url.scheme(), "socks5h");
        assert_eq!(url.host_str(), Some("proxy.example"));
        assert_eq!(url.port(), Some(1080));
        assert_eq!(url.username(), "user%20name");
        assert_eq!(url.password(), Some("p%40ss%3Aword"));

        let local = ResolvedProxy::Socks5 {
            host: SmolStr::new_static("[::1]"),
            port: 1080,
            credentials: None,
            resolve_dns_at_remote: false,
        };
        let url = Url::parse(&local.endpoint_url().expect("an endpoint")).unwrap();
        assert_eq!(url.scheme(), "socks5");
        assert_eq!(url.host_str(), Some("[::1]"));
        assert_eq!(url.port(), Some(1080));
        assert_eq!(url.username(), "");
    }

    #[test]
    fn http_endpoints_report_the_proxy_scheme() {
        let route = ResolvedProxy::Http {
            host: SmolStr::new_static("proxy.example"),
            port: 8443,
            credentials: None,
            tls_to_proxy: true,
        };
        let url = Url::parse(&route.endpoint_url().expect("an endpoint")).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("proxy.example"));
        assert_eq!(url.port(), Some(8443));

        assert_eq!(ResolvedProxy::NoProxy.endpoint_url(), None);
    }

    /// A venue that names a route is routed by it; one that names none follows the
    /// default, exactly as a Kytrix service without an override does.
    #[test]
    fn proxy_route_for_falls_back_to_the_default_route() {
        let target = target();
        let mut config = Config::default();
        assert_eq!(config.proxy_route_for(&venue()), ProxyRoute::Direct);
        assert_eq!(config.service_proxy_route_for(&target), ProxyRoute::Direct);

        config.proxy_default = ProxyRoute::System;
        config
            .proxy_applications
            .insert(target.clone(), ProxyRoute::named("hk"));
        config.venue_routes.insert(venue(), ProxyRoute::named("hk"));
        assert_eq!(
            config.service_proxy_route_for(&target),
            ProxyRoute::Named {
                name: SmolStr::new_static("hk"),
            }
        );
        assert_eq!(
            config.proxy_route_for(&venue()),
            ProxyRoute::Named {
                name: SmolStr::new_static("hk"),
            }
        );
        assert_eq!(
            config.proxy_route_for(&VenueId::Okx),
            ProxyRoute::System,
            "a venue without its own route follows the default"
        );
        assert_eq!(config.proxy_route_for(&venue()).named_name(), Some("hk"));
    }

    #[test]
    fn route_renaming_follows_the_catalog() {
        let mut route = ProxyRoute::named("old");
        route.rename_catalog_proxy("old", &SmolStr::new_static("new"));
        assert_eq!(route.named_name(), Some("new"));

        let mut direct = ProxyRoute::Direct;
        direct.rename_catalog_proxy("old", &SmolStr::new_static("new"));
        assert_eq!(direct, ProxyRoute::Direct);
    }

    /// A venue without its own generation follows the snapshot value.
    #[test]
    fn route_generations_default_to_the_snapshot_value() {
        let mut config = Config::default();
        assert_eq!(
            config.network_route_generation(&venue()),
            NetworkRouteGeneration::default()
        );

        config.default_route_generation = NetworkRouteGeneration::new(3);
        assert_eq!(config.network_route_generation(&venue()).get(), 3);

        config
            .route_generations
            .insert(venue(), NetworkRouteGeneration::new(9));
        assert_eq!(config.network_route_generation(&venue()).get(), 9);
    }

    /// A service target is the only target left: a venue is configured, not
    /// addressed.
    #[test]
    fn service_target_should_use_the_explicit_wire_shape() {
        assert_eq!(
            serde_json::to_value(target()).expect("the Kytrix target should serialize"),
            json!({ "kind": "kytrix_service", "capability": "x_account_monitoring_media" })
        );
        assert_eq!(
            serde_json::from_value::<ServiceTarget>(json!({
                "kind": "kytrix_service",
                "capability": "x_account_monitoring_media",
            }))
            .expect("the Kytrix target should decode"),
            target()
        );
        let strategic_maritime_chokepoints_tracking = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::StrategicMaritimeChokepointsTracking,
        };
        assert_eq!(
            serde_json::to_value(&strategic_maritime_chokepoints_tracking)
                .expect("the AIS target should serialize"),
            json!({ "kind": "kytrix_service", "capability": "strategic_maritime_chokepoints_tracking" })
        );
        assert_eq!(
            serde_json::from_value::<ServiceTarget>(json!({
                "kind": "kytrix_service",
                "capability": "strategic_maritime_chokepoints_tracking",
            }))
            .expect("the AIS target should decode"),
            strategic_maritime_chokepoints_tracking
        );
    }

    #[test]
    fn service_target_should_reject_the_removed_opaque_route() {
        for removed in [
            json!({ "kind": "route", "id": 1 }),
            json!({ "kind": "route", "id": 0 }),
            json!({
                "kind": "exchange",
                "venue": "binance",
                "market": "Spot",
                "capability": "candlesticks",
            }),
            json!({ "kind": "venue", "id": 1 }),
            json!({ "kind": "venue", "target": { "kind": "route", "id": 1 } }),
            json!({ "kind": "venue", "target": { "venue": "binance", "scope": "core/spot" } }),
            json!({ "kind": "kytrix_service", "capability": "chat" }),
        ] {
            assert!(
                serde_json::from_value::<ServiceTarget>(removed.clone()).is_err(),
                "{removed}"
            );
        }
    }

    /// A service target is an ordered key: a rewritten file keeps its order.
    #[test]
    fn service_target_should_keep_a_stable_order_as_a_btree_key() {
        let targets = [
            ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::XAccountMonitoringMedia,
            },
            ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::Account,
            },
            ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::CryptocurrencyWalletMonitoring,
            },
        ];
        let mut applications = BTreeMap::new();
        for target in targets.iter().rev() {
            applications.insert(target.clone(), ProxyRoute::named("hk"));
        }

        let mut expected = targets.to_vec();
        expected.sort();
        assert_eq!(
            applications.keys().cloned().collect::<Vec<_>>(),
            expected,
            "the map order is the target order, so a rewritten file stays stable"
        );
        assert_eq!(applications.len(), targets.len());
        assert_eq!(
            applications.get(&ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::CryptocurrencyWalletMonitoring,
            }),
            Some(&ProxyRoute::named("hk")),
            "a separate but equal target addresses the same entry"
        );
        assert_eq!(
            applications.get(&ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::News,
            }),
            None
        );
    }

    /// A venue that names a route and a generation of its own owns them; the
    /// venue that names none is the one that follows the snapshot's defaults.
    #[test]
    fn a_venue_owns_the_route_and_generation_it_names() {
        let binance = venue();
        let okx = VenueId::Okx;
        let mut config = Config {
            proxy_default: ProxyRoute::System,
            default_route_generation: NetworkRouteGeneration::new(7),
            ..Config::default()
        };
        config.venue_routes.insert(binance, ProxyRoute::named("hk"));
        config
            .route_generations
            .insert(binance, NetworkRouteGeneration::new(9));

        assert_eq!(config.proxy_route_for(&binance), ProxyRoute::named("hk"));
        assert_eq!(
            config.proxy_route_for(&okx),
            ProxyRoute::System,
            "a venue that names no route of its own follows the default"
        );
        assert_eq!(
            config.network_route_generation(&binance).get(),
            9,
            "the configured venue owns its generation"
        );
        assert_eq!(
            config.network_route_generation(&okx).get(),
            7,
            "a venue without one shares the snapshot generation"
        );
    }
}
