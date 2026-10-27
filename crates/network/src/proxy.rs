//! Network-behavior proxy routing.

use std::sync::Arc;

use app_config::network::{
    Config, ConfigError, NetworkRouteGeneration, ProxyRoute, ResolvedProxy, RouteTarget,
};
use market_participants_model::identifier::VenueId;
use tokio_util::sync::CancellationToken;
use url::Url;
use widget_tokio_runtime::runtime::Runtime;

use crate::system_proxy;

/// A resolved proxy together with the OS settings it was derived from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRoute {
    pub proxy: ResolvedProxy,
    pub os_proxy_fingerprint: u64,
}

/// Resolves one target's current route. Invalid bindings fail closed.
///
/// This is the only place Direct/Named/System choices are materialized into a
/// [`ResolvedProxy`]. Direct and Named never read the environment or OS proxy.
/// System evaluates the OS proxy, including PAC/WPAD, for `destination` — a
/// destination on this machine excepted, which the OS proxy never answers for.
pub fn resolve(
    config: &Config,
    target: &RouteTarget,
    destination: &Url,
) -> Result<ResolvedProxy, ConfigError> {
    resolve_route(config, target, destination).map(|route| route.proxy)
}

/// Returns the outbound route one target's requests take.
fn proxy_route(config: &Config, target: &RouteTarget) -> ProxyRoute {
    match target {
        RouteTarget::Venue(venue) => config.proxy_route_for(venue),
        RouteTarget::Service(service) => config.service_proxy_route_for(service),
    }
}

/// Returns the generation of the route one target's requests take.
fn route_generation(config: &Config, target: &RouteTarget) -> NetworkRouteGeneration {
    match target {
        RouteTarget::Venue(venue) => config.network_route_generation(venue),
        RouteTarget::Service(service) => config.service_network_route_generation(service),
    }
}

fn resolve_route(
    config: &Config,
    target: &RouteTarget,
    destination: &Url,
) -> Result<ResolvedRoute, ConfigError> {
    match proxy_route(config, target) {
        ProxyRoute::Direct => Ok(ResolvedRoute {
            proxy: ResolvedProxy::NoProxy,
            os_proxy_fingerprint: 0,
        }),
        ProxyRoute::Named { name } => Ok(ResolvedRoute {
            proxy: config.resolve_named_proxy(target, &name)?,
            os_proxy_fingerprint: 0,
        }),
        ProxyRoute::System => resolve_system_proxy(target, destination),
    }
}

/// Resolves a route without running system/PAC evaluation on a Tokio worker.
///
/// The synchronous resolver remains available for synchronous callers. System
/// discovery may perform blocking OS calls, download a PAC document, and
/// execute JavaScript, so async facades must cross this blocking boundary
/// before awaiting the result: they do it on `runtime`, never on the thread that
/// happens to be driving the caller's future.
pub async fn resolve_async(
    config: &Config,
    target: &RouteTarget,
    destination: &Url,
    runtime: &Runtime,
) -> Result<ResolvedProxy, ConfigError> {
    resolve_async_cancellable(
        config,
        target,
        destination,
        CancellationToken::new(),
        runtime,
    )
    .await
}

/// Async system-route resolution that can be abandoned when its operation is
/// cancelled. The blocking worker is detached by Tokio after cancellation;
/// importantly, no PAC/OS work remains on the async runtime worker.
pub async fn resolve_async_cancellable(
    config: &Config,
    target: &RouteTarget,
    destination: &Url,
    cancellation: CancellationToken,
    runtime: &Runtime,
) -> Result<ResolvedProxy, ConfigError> {
    resolve_route_async_cancellable(config, target, destination, cancellation, runtime)
        .await
        .map(|route| route.proxy)
}

/// Async route resolution that also reports the OS proxy fingerprint the route
/// was derived from.
pub async fn resolve_route_async_cancellable(
    config: &Config,
    target: &RouteTarget,
    destination: &Url,
    cancellation: CancellationToken,
    runtime: &Runtime,
) -> Result<ResolvedRoute, ConfigError> {
    if !matches!(proxy_route(config, target), ProxyRoute::System) {
        return resolve_route(config, target, destination);
    }

    let config = config.clone();
    let destination = destination.clone();
    let task_target = target.clone();
    // The runtime is the one that owns the blocking pool, so the OS calls and
    // the PAC evaluation run there whatever executor the caller is on.
    let task = runtime
        .clone()
        .blocking(move || resolve_route(&config, &task_target, &destination));
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ConfigError::InvalidSystemProxy { target: target.clone() }),
        result = task => result.unwrap_or(Err(ConfigError::InvalidSystemProxy { target: target.clone() })),
    }
}

/// Cheap OS-settings hash for System route fences. Direct/Named are `0`.
pub fn system_route_fingerprint(config: &Config, target: &RouteTarget) -> u64 {
    match proxy_route(config, target) {
        ProxyRoute::System => system_proxy::cached_os_proxy_fingerprint(),
        ProxyRoute::Direct | ProxyRoute::Named { .. } => 0,
    }
}

/// Refreshes the process-wide OS proxy fingerprint off the async runtime.
/// Session actors call this before comparing their cached System route fence.
pub async fn refresh_system_proxy_fingerprint(
    cancellation: CancellationToken,
    runtime: &Runtime,
) -> Option<u64> {
    system_proxy::refresh_os_proxy_fingerprint(cancellation, runtime).await
}

/// Route fence: System compares OS settings only; Direct/Named re-materialize.
pub fn session_route_matches(
    config: &Config,
    target: &RouteTarget,
    destination: &Url,
    stored_proxy: &ResolvedProxy,
    stored_fingerprint: u64,
) -> bool {
    if system_route_fingerprint(config, target) != stored_fingerprint {
        return false;
    }
    match proxy_route(config, target) {
        ProxyRoute::System => true,
        ProxyRoute::Direct | ProxyRoute::Named { .. } => {
            resolve(config, target, destination).as_ref() == Ok(stored_proxy)
        }
    }
}

fn resolve_system_proxy(
    target: &RouteTarget,
    destination: &Url,
) -> Result<ResolvedRoute, ConfigError> {
    system_proxy::read_os_proxy(target, destination).map(|(proxy, os_proxy_fingerprint)| {
        ResolvedRoute {
            proxy,
            os_proxy_fingerprint,
        }
    })
}

/// Resolves effective proxy routes from the owner's current network projection.
#[derive(Clone)]
pub struct ProxyResolver {
    config: Arc<dyn Fn() -> Arc<Config> + Send + Sync>,
}

impl ProxyResolver {
    /// Builds a resolver over `config`, which must answer the owner's latest
    /// complete network projection.
    pub fn new(config: Arc<dyn Fn() -> Arc<Config> + Send + Sync>) -> Self {
        Self { config }
    }

    /// Resolves one venue's current route and reports the generation it is on.
    ///
    /// A venue is routed by the venue's own configuration — the line it names, or
    /// the default line it names none of — so the venue is the subject: there is
    /// no service target standing in for it.
    pub async fn resolve_for_venue_with_generation(
        &self,
        venue: &VenueId,
        destination: &Url,
        runtime: &Runtime,
    ) -> Result<(ResolvedProxy, NetworkRouteGeneration), ConfigError> {
        self.resolve_for_target_with_generation(&RouteTarget::Venue(*venue), destination, runtime)
            .await
    }

    pub async fn resolve_for_target(
        &self,
        target: &RouteTarget,
        destination: &Url,
        runtime: &Runtime,
    ) -> Result<ResolvedProxy, ConfigError> {
        self.resolve_for_target_with_generation(target, destination, runtime)
            .await
            .map(|(proxy, _)| proxy)
    }

    pub async fn resolve_for_target_with_generation(
        &self,
        target: &RouteTarget,
        destination: &Url,
        runtime: &Runtime,
    ) -> Result<(ResolvedProxy, NetworkRouteGeneration), ConfigError> {
        let config = (self.config)();
        let generation = route_generation(&config, target);
        let proxy = resolve_async(&config, target, destination, runtime).await?;
        Ok((proxy, generation))
    }

    pub fn route_generation(&self, target: &RouteTarget) -> NetworkRouteGeneration {
        route_generation(&(self.config)(), target)
    }

    pub fn has_proxy_changed(
        &self,
        target: &RouteTarget,
        generation: NetworkRouteGeneration,
    ) -> bool {
        self.route_generation(target) != generation
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config::network::{
        Config, KytrixServiceCapability, NetworkRouteGeneration, ProxyAuthorizationConfig,
        ProxyConfig, ProxyRoute, ResolvedProxy, RouteTarget, ServiceTarget,
    };
    use smol_str::SmolStr;
    use url::Url;
    use widget_tokio_runtime::runtime::Runtime;

    use super::{ProxyResolver, resolve};

    #[test]
    fn resolve_should_materialize_direct_and_named_routes() {
        let target = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::XAccountMonitoringMedia,
        };
        let destination = Url::parse("https://pbs.twimg.com/media/test.jpg").unwrap();
        let mut config = Config::default();
        assert_eq!(
            resolve(&config, &RouteTarget::Service(target.clone()), &destination),
            Ok(ResolvedProxy::NoProxy)
        );

        config.proxies.insert(
            SmolStr::new_static("hk"),
            ProxyConfig {
                addr: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                authorization: ProxyAuthorizationConfig::None,
                resolve_dns_at_remote: true,
            },
        );
        config.proxy_default = ProxyRoute::named("hk");
        assert_eq!(
            resolve(&config, &RouteTarget::Service(target), &destination),
            Ok(ResolvedProxy::Socks5 {
                host: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                credentials: None,
                resolve_dns_at_remote: true,
            })
        );
    }

    #[tokio::test]
    async fn resolver_materializes_direct_and_named_routes() {
        let target = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::XAccountMonitoringMedia,
        };
        let destination = Url::parse("https://pbs.twimg.com/media/test.jpg").unwrap();
        let application = Runtime::for_test();
        let mut config = Config::default();
        let current = Arc::new(std::sync::RwLock::new(Arc::new(Config::default())));
        let resolver = ProxyResolver::new({
            let current = Arc::clone(&current);
            Arc::new(move || {
                current
                    .read()
                    .expect("the test lock is not poisoned")
                    .clone()
            })
        });

        assert_eq!(
            resolver
                .resolve_for_target(
                    &RouteTarget::Service(target.clone()),
                    &destination,
                    &application
                )
                .await
                .unwrap(),
            ResolvedProxy::NoProxy
        );

        config.proxies.insert(
            SmolStr::new_static("local"),
            ProxyConfig {
                addr: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                authorization: ProxyAuthorizationConfig::None,
                resolve_dns_at_remote: true,
            },
        );
        config.proxy_default = ProxyRoute::named("local");
        config.default_route_generation = NetworkRouteGeneration::new(7);
        *current.write().expect("the test lock is not poisoned") = Arc::new(config.clone());
        assert_eq!(
            resolver
                .resolve_for_target(
                    &RouteTarget::Service(target.clone()),
                    &destination,
                    &application
                )
                .await
                .unwrap(),
            ResolvedProxy::Socks5 {
                host: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                credentials: None,
                resolve_dns_at_remote: true,
            }
        );
        assert_eq!(
            resolver
                .resolve_for_target_with_generation(
                    &RouteTarget::Service(target.clone()),
                    &destination,
                    &application
                )
                .await
                .unwrap()
                .1,
            NetworkRouteGeneration::new(7),
            "the resolver reports the typed generation of the route it resolved"
        );
    }
}
