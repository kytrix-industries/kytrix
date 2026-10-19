//! What one adapter execution is given besides the request itself.
//!
//! The execution context is built by the gateway from the venue configuration of
//! the route the request was validated against. Its only route identity is the
//! [`RequestIdentity`]: the route and the capability are stated together, so a
//! context can never disagree with the descriptor that produced it. Its origins
//! are the **venue's** own — the pair the venue's configuration gives this route's
//! scope, or the venue's default for a scope the configuration does not name — so
//! an adapter never picks a host from a venue name or from an endpoint id.
//!
//! One conversation is not about a route at all: the [`VenueExecutionContext`],
//! which a load opens to ask a venue what it publishes. It names the venue instead
//! of a route, and it dials the venue's **primary scope**, because the read covers
//! every scope the venue may publish rather than one of them.

use std::{fmt, sync::Arc};

use app_config::{
    market_participants::origins::OriginPair,
    network::{NetworkRouteGeneration, ResolvedProxy},
};
use app_config_provider::notification::ConfigNotification;
use market_participants_model::{
    identifier::VenueId, listing::route::VenueRoute, routing::CapabilityId,
};
use reqwest::Client;
use smol_str::SmolStr;
use url::Url;

use crate::{
    connection::ConnectionPool, protocol::RequestIdentity, routing::CredentialGeneration,
    runtime::MarketParticipantNetworkSnapshot, venue::metadata::VenueMetadata,
};

#[derive(Clone, PartialEq, Eq)]
pub enum Authentication {
    /// An API key signed with a shared secret.
    ApiKey {
        account_id: SmolStr,
        api_key: SmolStr,
        secret: SmolStr,
    },
    /// An API key of a venue that authenticates with a passphrase as well.
    ///
    /// The passphrase is part of the credential rather than a field a venue may
    /// ignore: a venue that requires one cannot be signed without it, so an
    /// execution authenticated this way carries it and nothing has to look it up
    /// again from the venue's stored credentials.
    PassphraseApiKey {
        account_id: SmolStr,
        api_key: SmolStr,
        secret: SmolStr,
        passphrase: SmolStr,
    },
    /// A credential set a venue authenticates with by an API key pair and a bearer token.
    ///
    /// The token is part of the credential rather than a field a venue may ignore: the
    /// venue hands out a session from it and refuses every command without one, so an
    /// execution authenticated this way carries it.
    LongbridgeApiKey {
        account_id: SmolStr,
        app_key: SmolStr,
        app_secret: SmolStr,
        access_token: SmolStr,
    },
    Wallet {
        account_id: SmolStr,
        address: SmolStr,
        private_key: SmolStr,
    },
}

impl fmt::Debug for Authentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Authentication");
        match self {
            Self::ApiKey { account_id, .. } => debug
                .field("kind", &"ApiKey")
                .field("account_id", account_id)
                .field("api_key", &"<redacted>")
                .field("secret", &"<redacted>"),
            Self::PassphraseApiKey { account_id, .. } => debug
                .field("kind", &"PassphraseApiKey")
                .field("account_id", account_id)
                .field("api_key", &"<redacted>")
                .field("secret", &"<redacted>")
                .field("passphrase", &"<redacted>"),
            Self::LongbridgeApiKey { account_id, .. } => debug
                .field("kind", &"LongbridgeApiKey")
                .field("account_id", account_id)
                .field("app_key", &"<redacted>")
                .field("app_secret", &"<redacted>")
                .field("access_token", &"<redacted>"),
            Self::Wallet {
                account_id,
                address,
                ..
            } => debug
                .field("kind", &"Wallet")
                .field("account_id", account_id)
                .field("address", address)
                .field("private_key", &"<redacted>"),
        }
        .finish()
    }
}

impl Authentication {
    pub fn account_id(&self) -> &str {
        match self {
            Self::ApiKey { account_id, .. }
            | Self::PassphraseApiKey { account_id, .. }
            | Self::LongbridgeApiKey { account_id, .. }
            | Self::Wallet { account_id, .. } => account_id,
        }
    }
}

/// Everything one adapter execution needs that is not the request.
#[derive(Clone)]
pub struct ExecutionContext {
    /// The only route and capability identity of this execution.
    identity: RequestIdentity,
    /// The origins the route's venue configuration gives its scope.
    origins: OriginPair,
    pub auth: Option<Authentication>,
    pub proxy: ResolvedProxy,
    /// Generation of the resolved network route used for this operation.
    ///
    /// The generation is part of connection identity so a proxy change cannot
    /// accidentally reuse a live socket created for an older route.
    network_route_generation: NetworkRouteGeneration,
    /// Generation of the credentials used by authenticated requests.
    credential_generation: Option<CredentialGeneration>,
    pub http_client: Client,
    pub connection_pool: Arc<ConnectionPool>,
    pub runtime: Option<ConfigNotification<MarketParticipantNetworkSnapshot>>,
    /// The venue-level route metadata standing behind this execution.
    ///
    /// A request path carries the metadata of the loaded venue it was admitted
    /// against: it is read from the commit that admitted the request, never
    /// fetched here, so no request path can reach the venue for metadata. A
    /// load-time conversation carries what that same load read — which is what
    /// lets a catalog be staged from the metadata its own commit will publish.
    /// Absent means no venue reading stands behind the execution at all, which is
    /// the discovery conversation itself.
    metadata: Option<Arc<VenueMetadata>>,
}

impl fmt::Debug for ExecutionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionContext")
            .field("identity", &self.identity)
            .field("origins", &self.origins)
            .field("auth", &self.auth.as_ref().map(Authentication::account_id))
            .field("proxy", &self.proxy)
            .field("network_route_generation", &self.network_route_generation)
            .field("credential_generation", &self.credential_generation)
            .finish_non_exhaustive()
    }
}

impl ExecutionContext {
    /// Builds the context of one validated request.
    pub(crate) fn new(
        identity: RequestIdentity,
        origins: OriginPair,
        http_client: Client,
        connection_pool: Arc<ConnectionPool>,
    ) -> Self {
        Self {
            identity,
            origins,
            auth: None,
            proxy: ResolvedProxy::NoProxy,
            network_route_generation: NetworkRouteGeneration::default(),
            credential_generation: None,
            http_client,
            connection_pool,
            runtime: None,
            metadata: None,
        }
    }

    /// Attaches the venue metadata this execution is resolved against.
    pub(crate) fn with_venue_metadata(mut self, metadata: Option<Arc<VenueMetadata>>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Returns the venue metadata this execution is resolved against.
    ///
    /// Nothing here fetches: a request path is handed the metadata of the commit
    /// that admitted it, and an adapter that needs a scope's metadata reads it
    /// from this value or refuses the request.
    pub(crate) fn venue_metadata(&self) -> Option<&Arc<VenueMetadata>> {
        self.metadata.as_ref()
    }

    /// Returns the identity this execution acts under.
    pub const fn identity(&self) -> &RequestIdentity {
        &self.identity
    }

    /// Returns the venue and scope of this execution.
    pub fn route(&self) -> VenueRoute {
        self.identity().route().clone()
    }

    /// Returns the capability of this execution.
    pub const fn capability(&self) -> CapabilityId {
        self.identity().capability()
    }

    /// Returns the HTTP origin the route's venue configuration gives its scope.
    pub(crate) fn http_origin(&self) -> &Url {
        &self.origins.http
    }

    /// Returns the WebSocket origin the route's venue configuration gives its scope.
    pub(crate) fn websocket_origin(&self) -> &Url {
        &self.origins.websocket
    }

    /// Returns the generation of the resolved network route.
    pub const fn network_route_generation(&self) -> NetworkRouteGeneration {
        self.network_route_generation
    }

    /// Returns the generation of the credentials this execution signs with.
    pub const fn credential_generation(&self) -> Option<CredentialGeneration> {
        self.credential_generation
    }

    pub fn with_authentication(mut self, auth: Authentication) -> Self {
        self.auth = Some(auth);
        self
    }

    pub(crate) fn with_proxy(
        mut self,
        proxy: ResolvedProxy,
        network_route_generation: NetworkRouteGeneration,
    ) -> Self {
        self.proxy = proxy;
        self.network_route_generation = network_route_generation;
        self
    }

    pub(crate) fn with_credential_generation(
        mut self,
        generation: Option<CredentialGeneration>,
    ) -> Self {
        self.credential_generation = generation;
        self
    }

    pub(crate) fn with_runtime(
        mut self,
        runtime: ConfigNotification<MarketParticipantNetworkSnapshot>,
    ) -> Self {
        self.runtime = Some(runtime);
        self
    }

    pub(crate) fn current_runtime(&self) -> Option<Arc<MarketParticipantNetworkSnapshot>> {
        self.runtime.as_ref().map(ConfigNotification::current)
    }
}

/// Everything one venue-level read of one venue needs.
///
/// The discovery conversation asks a venue what it publishes before anything of it
/// is registered, so it addresses the venue rather than a route: there is no
/// [`RequestIdentity`] to carry, and the origins are the venue's **default** pair —
/// the one a scope the configuration does not name is reached on — because the read
/// covers every scope the venue may publish rather than one of them.
#[derive(Clone)]
pub struct VenueExecutionContext {
    /// The venue this read asks.
    venue: VenueId,
    /// The pair the venue's own configuration gives a scope it does not name.
    origins: OriginPair,
    pub http_client: Client,
}

impl VenueExecutionContext {
    /// Builds the context of one venue-level read.
    pub(crate) fn new(venue: VenueId, origins: OriginPair, http_client: Client) -> Self {
        Self {
            venue,
            origins,
            http_client,
        }
    }

    /// Returns the venue this read asks.
    pub const fn venue(&self) -> &VenueId {
        &self.venue
    }

    /// Returns the HTTP origin this read dials.
    pub(crate) fn http_origin(&self) -> &Url {
        &self.origins.http
    }
}

#[cfg(test)]
mod tests {
    use app_config::market_participants::origins::OriginPair;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        routing::CapabilityId,
    };

    use super::{Authentication, ExecutionContext};
    use crate::{connection::ConnectionPool, protocol::RequestIdentity};

    #[test]
    fn authentication_debug_redacts_credentials() {
        let api_key = Authentication::ApiKey {
            account_id: "account".into(),
            api_key: "api-key-secret".into(),
            secret: "signing-secret".into(),
        };
        let wallet = Authentication::Wallet {
            account_id: "wallet-account".into(),
            address: "0xabc".into(),
            private_key: "private-key-secret".into(),
        };
        let longbridge = Authentication::LongbridgeApiKey {
            account_id: "longbridge-account".into(),
            app_key: "longbridge-app-key-secret".into(),
            app_secret: "longbridge-app-secret-secret".into(),
            access_token: "longbridge-access-token-secret".into(),
        };

        let api_debug = format!("{api_key:?}");
        let wallet_debug = format!("{wallet:?}");
        let longbridge_debug = format!("{longbridge:?}");
        assert!(api_debug.contains("account"));
        assert!(api_debug.contains("<redacted>"));
        assert!(!api_debug.contains("api-key-secret"));
        assert!(!api_debug.contains("signing-secret"));
        assert!(wallet_debug.contains("0xabc"));
        assert!(wallet_debug.contains("<redacted>"));
        assert!(!wallet_debug.contains("private-key-secret"));
        assert!(longbridge_debug.contains("longbridge-account"));
        assert!(longbridge_debug.contains("<redacted>"));
        assert!(!longbridge_debug.contains("longbridge-app-key-secret"));
        assert!(!longbridge_debug.contains("longbridge-app-secret-secret"));
        assert!(!longbridge_debug.contains("longbridge-access-token-secret"));
        assert_eq!(longbridge.account_id(), "longbridge-account");
    }

    fn context() -> ExecutionContext {
        let transport = std::sync::Arc::new(network::transport::hub::TransportHub::new(
            network::transport::hub::TransportPolicy::default(),
        ));
        let pool = std::sync::Arc::new(ConnectionPool::with_transport(transport));
        let client = reqwest::Client::new();
        let route = market_participants_model::listing::route::VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new("spot").expect("the scope should be canonical"),
        );
        let identity = RequestIdentity::new(route, CapabilityId::Candlesticks);
        let origins = OriginPair {
            http: "https://api.binance.com"
                .parse()
                .expect("the origin parses"),
            websocket: "wss://stream.binance.com:9443"
                .parse()
                .expect("the origin parses"),
        };
        ExecutionContext::new(identity, origins, client, pool)
    }

    /// The identity is the only route identity a context has: the route and the
    /// capability are stated together, so the two can never disagree.
    #[test]
    fn the_route_and_capability_are_stated_together() {
        let context = context();

        assert_eq!(context.route().venue.as_str(), "binance");
        assert_eq!(context.route().scope.as_str(), "spot");
        assert_eq!(context.capability(), CapabilityId::Candlesticks);
        assert_eq!(context.identity().capability(), CapabilityId::Candlesticks);
        assert_eq!(
            context.http_origin().as_str(),
            "https://api.binance.com/",
            "the origins are the venue's own"
        );
        assert_eq!(
            context.websocket_origin().as_str(),
            "wss://stream.binance.com:9443/"
        );
    }

    /// A context defaults to no proxy decision, no credentials and generation
    /// zero; the gateway replaces all three from the venue configuration it resolved.
    #[test]
    fn a_fresh_context_carries_no_proxy_or_credential_generation() {
        let context = context();

        assert_eq!(context.credential_generation(), None);
        assert_eq!(
            context.network_route_generation(),
            app_config::network::NetworkRouteGeneration::default()
        );
    }
}
