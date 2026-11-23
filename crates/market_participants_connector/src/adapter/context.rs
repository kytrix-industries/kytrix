use std::{fmt, sync::Arc};

use app_config::{
    market_participants::origins::VenueEndpoints,
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
    ApiKey {
        account_id: SmolStr,
        api_key: SmolStr,
        secret: SmolStr,
    },
    PassphraseApiKey {
        account_id: SmolStr,
        api_key: SmolStr,
        secret: SmolStr,
        passphrase: SmolStr,
    },
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

#[derive(Clone)]
pub struct ExecutionContext {
    identity: RequestIdentity,
    endpoints: VenueEndpoints,
    pub auth: Option<Authentication>,
    pub proxy: ResolvedProxy,
    network_route_generation: NetworkRouteGeneration,
    credential_generation: Option<CredentialGeneration>,
    pub http_client: Client,
    pub connection_pool: Arc<ConnectionPool>,
    pub runtime: Option<ConfigNotification<MarketParticipantNetworkSnapshot>>,
    metadata: Option<Arc<VenueMetadata>>,
}

impl fmt::Debug for ExecutionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionContext")
            .field("identity", &self.identity)
            .field("endpoints", &self.endpoints)
            .field("auth", &self.auth.as_ref().map(Authentication::account_id))
            .field("proxy", &self.proxy)
            .field("network_route_generation", &self.network_route_generation)
            .field("credential_generation", &self.credential_generation)
            .finish_non_exhaustive()
    }
}

impl ExecutionContext {
    #[cfg(test)]
    pub(crate) fn new(
        identity: RequestIdentity,
        origins: app_config::market_participants::origins::OriginPair,
        http_client: Client,
        connection_pool: Arc<ConnectionPool>,
    ) -> Self {
        Self::new_with_endpoints(
            identity,
            VenueEndpoints {
                primary: origins,
                metadata: None,
            },
            http_client,
            connection_pool,
        )
    }

    pub(crate) fn new_with_endpoints(
        identity: RequestIdentity,
        endpoints: VenueEndpoints,
        http_client: Client,
        connection_pool: Arc<ConnectionPool>,
    ) -> Self {
        Self {
            identity,
            endpoints,
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

    pub(crate) fn with_venue_metadata(mut self, metadata: Option<Arc<VenueMetadata>>) -> Self {
        self.metadata = metadata;
        self
    }

    pub(crate) fn venue_metadata(&self) -> Option<&Arc<VenueMetadata>> {
        self.metadata.as_ref()
    }

    pub const fn identity(&self) -> &RequestIdentity {
        &self.identity
    }

    pub fn route(&self) -> VenueRoute {
        self.identity().route().clone()
    }

    pub const fn capability(&self) -> CapabilityId {
        self.identity().capability()
    }

    pub(crate) fn http_origin(&self) -> &Url {
        &self.endpoints.primary.http
    }

    pub(crate) fn http_metadata(&self) -> &Url {
        self.endpoints
            .metadata
            .as_ref()
            .map_or(&self.endpoints.primary.http, |service| &service.http)
    }

    pub(crate) fn websocket_origin(&self) -> &Url {
        &self.endpoints.primary.websocket
    }

    pub const fn network_route_generation(&self) -> NetworkRouteGeneration {
        self.network_route_generation
    }

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

#[derive(Clone)]
pub struct VenueExecutionContext {
    venue: VenueId,
    endpoints: VenueEndpoints,
    pub http_client: Client,
}

impl VenueExecutionContext {
    #[cfg(test)]
    pub(crate) fn new(
        venue: VenueId,
        origins: app_config::market_participants::origins::OriginPair,
        http_client: Client,
    ) -> Self {
        Self::new_with_endpoints(
            venue,
            VenueEndpoints {
                primary: origins,
                metadata: None,
            },
            http_client,
        )
    }

    pub(crate) fn new_with_endpoints(
        venue: VenueId,
        endpoints: VenueEndpoints,
        http_client: Client,
    ) -> Self {
        Self {
            venue,
            endpoints,
            http_client,
        }
    }

    pub const fn venue(&self) -> &VenueId {
        &self.venue
    }

    pub(crate) fn http_origin(&self) -> &Url {
        &self.endpoints.primary.http
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
