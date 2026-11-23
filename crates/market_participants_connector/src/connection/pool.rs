use std::sync::Arc;

use ahash::AHashMap;
use network::transport::hub::TransportHub;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::physical::{ConnectionConnectOptions, ConnectionProtocol};
use crate::{
    connection::{Connection, ConnectionKey, ConnectionState},
    error::MarketParticipantError,
    runtime::MarketParticipantNetworkSnapshot,
};

#[derive(Clone)]
pub struct ConnectionPool {
    transport: Arc<TransportHub>,
    connections: Arc<RwLock<AHashMap<ConnectionKey, Arc<Connection>>>>,
}

struct PoolConnectOptions {
    endpoint: url::Url,
    state: ConnectionState,
    protocol: ConnectionProtocol,
    cancellation: CancellationToken,
    reconnect: crate::connection::ReconnectPolicy,
    reauthentication: Option<crate::connection::Reauthentication>,
}

pub type SessionCredential = smol_str::SmolStr;

pub type Handshaken = (
    crate::connection::Reauthentication,
    Option<SessionCredential>,
);

enum Handshake {
    Fetch(
        Box<
            dyn FnOnce() -> futures::future::BoxFuture<
                    'static,
                    Result<Handshaken, MarketParticipantError>,
                > + Send,
        >,
    ),
}

impl Handshake {
    async fn run(self) -> Result<Handshaken, MarketParticipantError> {
        match self {
            Self::Fetch(fetch) => fetch().await,
        }
    }
}

impl ConnectionPool {
    pub fn with_transport(transport: Arc<TransportHub>) -> Self {
        Self {
            transport,
            connections: Arc::new(RwLock::new(AHashMap::with_capacity(8))),
        }
    }

    pub fn transport(&self) -> Arc<TransportHub> {
        Arc::clone(&self.transport)
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.transport.cancellation().child_token()
    }

    #[cfg(test)]
    pub(crate) async fn get_or_create_fixture(
        &self,
        key: ConnectionKey,
        state: ConnectionState,
    ) -> Arc<Connection> {
        let mut connections = self.connections.write().await;
        if let Some(connection) = connections.get(&key).cloned()
            && connection.is_healthy()
        {
            return connection;
        }
        if let Some(previous) = connections.remove(&key) {
            previous.close();
        }
        let (outbound, _) = tokio::sync::mpsc::channel(1);
        let (subscription_outbound, _) = tokio::sync::mpsc::unbounded_channel();
        let connection =
            Connection::with_outbound(key.clone(), state, outbound, subscription_outbound);
        connections.insert(key, connection.clone());
        connection
    }

    pub async fn connect_or_create(
        &self,
        key: ConnectionKey,
        state: ConnectionState,
        protocol: ConnectionProtocol,
        cancellation: CancellationToken,
        reconnect: crate::connection::ReconnectPolicy,
    ) -> Result<Arc<Connection>, MarketParticipantError> {
        self.connect_or_create_with_reauthentication(
            key,
            state,
            protocol,
            cancellation,
            reconnect,
            None,
        )
        .await
    }

    pub async fn connect_or_create_with_reauthentication(
        &self,
        key: ConnectionKey,
        state: ConnectionState,
        protocol: ConnectionProtocol,
        cancellation: CancellationToken,
        reconnect: crate::connection::ReconnectPolicy,
        reauthentication: Option<crate::connection::Reauthentication>,
    ) -> Result<Arc<Connection>, MarketParticipantError> {
        let endpoint = key.endpoint.clone();
        self.connect_or_create_with_reauthentication_and_endpoint(
            key,
            PoolConnectOptions {
                endpoint,
                state,
                protocol,
                cancellation,
                reconnect,
                reauthentication,
            },
            None,
        )
        .await
        .map(|(connection, _, _)| connection)
    }

    pub async fn connect_or_create_by_handshake<F>(
        &self,
        key: ConnectionKey,
        state: ConnectionState,
        protocol: ConnectionProtocol,
        cancellation: CancellationToken,
        reconnect: crate::connection::ReconnectPolicy,
        handshake: F,
    ) -> Result<(Arc<Connection>, Option<SessionCredential>), MarketParticipantError>
    where
        F: FnOnce() -> futures::future::BoxFuture<
                'static,
                Result<Handshaken, MarketParticipantError>,
            > + Send
            + 'static,
    {
        let endpoint = key.endpoint.clone();
        let (connection, _created, credential) = self
            .connect_or_create_with_reauthentication_and_endpoint(
                key,
                PoolConnectOptions {
                    endpoint,
                    state,
                    protocol,
                    cancellation,
                    reconnect,
                    reauthentication: None,
                },
                Some(Handshake::Fetch(Box::new(handshake))),
            )
            .await?;
        Ok((connection, credential))
    }

    pub(crate) async fn connect_or_create_with_endpoint(
        &self,
        key: ConnectionKey,
        endpoint: url::Url,
        state: ConnectionState,
        protocol: ConnectionProtocol,
        cancellation: CancellationToken,
        reconnect: crate::connection::ReconnectPolicy,
    ) -> Result<(Arc<Connection>, bool), MarketParticipantError> {
        self.connect_or_create_with_reauthentication_and_endpoint(
            key,
            PoolConnectOptions {
                endpoint,
                state,
                protocol,
                cancellation,
                reconnect,
                reauthentication: None,
            },
            None,
        )
        .await
        .map(|(connection, created, _)| (connection, created))
    }

    async fn connect_or_create_with_reauthentication_and_endpoint(
        &self,
        key: ConnectionKey,
        options: PoolConnectOptions,
        handshake: Option<Handshake>,
    ) -> Result<(Arc<Connection>, bool, Option<SessionCredential>), MarketParticipantError> {
        let PoolConnectOptions {
            endpoint,
            state,
            protocol,
            cancellation,
            reconnect,
            reauthentication,
        } = options;
        if let Some(connection) = self.connections.read().await.get(&key).cloned()
            && connection.is_healthy()
        {
            if let Some(reauthentication) = reauthentication.as_ref() {
                connection.set_reauthentication(reauthentication.clone());
            }
            return Ok((connection, false, None));
        }
        let (reauthentication, credential) = match handshake {
            Some(handshake) => {
                let (reauthentication, credential) = handshake.run().await?;
                (Some(reauthentication), credential)
            }
            None => (reauthentication, None),
        };
        let connection = Connection::connect_with_endpoint(ConnectionConnectOptions {
            endpoint,
            key: key.clone(),
            state,
            protocol,
            cancellation,
            reconnect,
            reauthentication,
            transport: Arc::clone(&self.transport),
        })
        .await?;
        let mut connections = self.connections.write().await;
        if let Some(existing) = connections.get(&key).cloned()
            && existing.is_healthy()
        {
            connection.close();
            return Ok((existing, false, None));
        }
        if let Some(previous) = connections.insert(key, connection.clone()) {
            if previous.has_subscriptions() {
                previous.retire();
            } else {
                previous.close();
            }
        }
        Ok((connection, true, credential))
    }

    pub async fn get(&self, key: &ConnectionKey) -> Option<Arc<Connection>> {
        self.connections.read().await.get(key).cloned()
    }

    pub async fn close_all(&self) {
        let mut connections = self.connections.write().await;
        for (_, connection) in connections.drain() {
            connection.close();
        }
    }

    pub async fn remove(&self, key: &ConnectionKey) -> Option<Arc<Connection>> {
        let connection = self.connections.write().await.remove(key);
        if let Some(connection) = &connection {
            connection.close();
        }
        connection
    }

    pub async fn invalidate_stale(&self, config: &MarketParticipantNetworkSnapshot) {
        let stale = self
            .connections
            .read()
            .await
            .keys()
            .filter_map(|key| {
                let route = &key.route;
                let network_route_generation = config.network_route_generation(&route.venue);
                let origin_stale = !config.serves(&route.venue, &route.scope, &key.endpoint);
                let credential_generation =
                    config.credential_generation(&route.venue, &route.scope);
                (key.network_route_generation != network_route_generation
                    || origin_stale
                    || key.credential_generation != credential_generation)
                    .then(|| key.clone())
            })
            .collect::<Vec<_>>();
        for key in stale {
            self.remove(&key).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use app_config::{
        market_participants::{
            credentials::{BinanceMarketCredentials, OkxCredentials},
            fixture::{self, FixtureAddress},
            origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
            settings::BinanceOrigins,
        },
        network::{NetworkRouteGeneration, NetworkRuntimeGenerations, ResolvedProxy},
        settings::Config,
    };
    use bytes::Bytes;
    use hyper::header::{HeaderMap, HeaderName, HeaderValue};
    use market_participants_model::{
        identifier::{MarketParticipantAccountId, VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };
    use network::transport::hub::TransportHub;
    use tokio::time::{Duration, timeout};
    use url::Url;

    use super::ConnectionPool;
    use crate::{
        connection::{
            Connection, ConnectionKey, ConnectionState, ReconnectPolicy, SubscriptionTopic,
            key::UpgradeHeaders,
            physical::{ConnectionProtocol, no_response_id},
        },
        routing::CredentialGeneration,
        runtime::MarketParticipantNetworkSnapshot,
        test_support::{InfoSocket, fixture_config, plaintext_policy},
    };

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the venue should be canonical")
    }

    fn scope(name: &str) -> VenueScopeId {
        VenueScopeId::new(name).expect("the scope should be canonical")
    }

    fn route(venue_name: &str, scope_name: &str) -> VenueRoute {
        VenueRoute::new(venue(venue_name), scope(scope_name))
    }

    fn binance_route() -> VenueRoute {
        route("binance", "spot")
    }

    fn okx_route() -> VenueRoute {
        route("okx", "spot")
    }

    fn stream_host(snapshot: &MarketParticipantNetworkSnapshot, route: &VenueRoute) -> Url {
        snapshot
            .origins(&route.venue, &route.scope)
            .expect("a built-in venue is configured")
            .websocket
    }

    fn request_host(snapshot: &MarketParticipantNetworkSnapshot, route: &VenueRoute) -> Url {
        snapshot
            .origins(&route.venue, &route.scope)
            .expect("a built-in venue is configured")
            .http
    }

    fn configured_origin(snapshot: &MarketParticipantNetworkSnapshot, route: &VenueRoute) -> Url {
        snapshot
            .origins(&route.venue, &route.scope)
            .expect("a built-in venue is configured")
            .websocket
    }

    fn fixture_key(
        snapshot: &MarketParticipantNetworkSnapshot,
        route: &VenueRoute,
    ) -> ConnectionKey {
        fixture_key_declaring(snapshot, route, UpgradeHeaders::default())
    }

    fn fixture_key_declaring(
        snapshot: &MarketParticipantNetworkSnapshot,
        route: &VenueRoute,
        upgrade_headers: UpgradeHeaders,
    ) -> ConnectionKey {
        let protocol =
            ConnectionProtocol::new(no_response_id).with_upgrade_headers(upgrade_headers);
        ConnectionKey::for_route(
            configured_origin(snapshot, route),
            ResolvedProxy::NoProxy,
            None,
            route.clone(),
            snapshot.network_route_generation(&route.venue),
            &protocol,
        )
    }

    fn pool() -> ConnectionPool {
        ConnectionPool::with_transport(Arc::new(TransportHub::new(plaintext_policy())))
    }

    fn generations(venues: impl IntoIterator<Item = (VenueId, u64)>) -> NetworkRuntimeGenerations {
        NetworkRuntimeGenerations {
            default_route: NetworkRouteGeneration::new(1),
            venues: venues
                .into_iter()
                .map(|(venue, generation)| (venue, NetworkRouteGeneration::new(generation)))
                .collect(),
            services: BTreeMap::new(),
        }
    }

    fn binance_credentials(config: &mut Config, api_key: &str, private_key: &str) {
        assert!(config.set_binance_market_credentials(
            &venue("binance"),
            BinanceMarket::Spot,
            Some(BinanceMarketCredentials {
                api_key: api_key.into(),
                private_key: private_key.into(),
                passphrase: "".into(),
            }),
        ));
    }

    fn okx_credentials(config: &mut Config, api_key: &str, secret_key: &str) {
        assert!(config.set_okx_credentials(
            &venue("okx"),
            Some(OkxCredentials {
                api_key: api_key.into(),
                secret_key: secret_key.into(),
                passphrase: "pass".into(),
            }),
        ));
    }

    fn fixture_address(port: u16) -> FixtureAddress {
        let address =
            Url::parse(&format!("http://127.0.0.1:{port}")).expect("the fixture address is a URL");
        FixtureAddress::at(&address)
    }

    async fn fixture_connection(
        pool: &ConnectionPool,
        snapshot: &MarketParticipantNetworkSnapshot,
        route: &VenueRoute,
    ) -> Arc<Connection> {
        pool.get_or_create_fixture(fixture_key(snapshot, route), ConnectionState::stateless())
            .await
    }

    #[tokio::test]
    async fn pool_reuses_healthy_and_replaces_closed_connections() {
        let pool = pool();
        let snapshot = MarketParticipantNetworkSnapshot::new(Config::default());
        let key = fixture_key(&snapshot, &binance_route());
        let first = pool
            .get_or_create_fixture(key.clone(), ConnectionState::stateless())
            .await;
        let second = pool
            .get_or_create_fixture(key.clone(), ConnectionState::stateless())
            .await;
        assert!(Arc::ptr_eq(&first, &second));
        first.close();
        let replacement = pool
            .get_or_create_fixture(key, ConnectionState::stateless())
            .await;
        assert!(!Arc::ptr_eq(&first, &replacement));
    }

    #[tokio::test]
    async fn two_upgrade_header_declarations_are_two_pool_entries() {
        let pool = pool();
        let snapshot = MarketParticipantNetworkSnapshot::new(Config::default());
        let route = binance_route();
        let asia = fixture_key_declaring(
            &snapshot,
            &route,
            declared_headers(&[("x-dc-region", "ap")]),
        );
        let united_states = fixture_key_declaring(
            &snapshot,
            &route,
            declared_headers(&[("x-dc-region", "us")]),
        );
        assert_ne!(
            asia, united_states,
            "one origin served two ways is two connections"
        );

        let asian = pool
            .get_or_create_fixture(asia.clone(), ConnectionState::stateless())
            .await;
        let american = pool
            .get_or_create_fixture(united_states.clone(), ConnectionState::stateless())
            .await;

        assert!(
            !Arc::ptr_eq(&asian, &american),
            "a request declaring one region is not answered from the socket dialled for another"
        );
        assert!(
            pool.get(&asia).await.is_some() && pool.get(&united_states).await.is_some(),
            "both declarations keep their own entry"
        );
    }

    fn declared_headers(headers: &[(&str, &str)]) -> UpgradeHeaders {
        let mut declared = HeaderMap::new();
        for (name, value) in headers {
            declared.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("the header name should be valid"),
                HeaderValue::from_str(value).expect("the header value should be valid"),
            );
        }
        UpgradeHeaders::from_header_map(&declared)
    }

    #[tokio::test]
    async fn invalidate_stale_closes_only_connections_for_changed_venues() {
        let mut config = Config::default();
        config.set_network_generations(generations([(venue("binance"), 41)]));
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());
        let pool = pool();
        let binance = fixture_connection(&pool, &initial, &binance_route()).await;
        let okx = fixture_connection(&pool, &initial, &okx_route()).await;

        config.set_network_generations(generations([(venue("binance"), 77)]));
        let updated = MarketParticipantNetworkSnapshot::from_previous(config, Some(&initial));
        pool.invalidate_stale(&updated).await;

        assert!(binance.is_closed(), "a changed venue should be closed");
        assert!(
            !okx.is_closed(),
            "another venue keeps its connection: generations are per venue"
        );
    }

    #[tokio::test]
    async fn invalidate_stale_closes_only_the_venue_that_rotated() {
        let initial = MarketParticipantNetworkSnapshot::new(Config::default());
        let pool = pool();
        let binance = fixture_connection(&pool, &initial, &binance_route()).await;
        let okx = fixture_connection(&pool, &initial, &okx_route()).await;

        let installation = fixture::venues([(venue("okx"), fixture_address(9077))]);
        let updated = MarketParticipantNetworkSnapshot::from_previous(
            fixture_config(&installation),
            Some(&initial),
        );
        pool.invalidate_stale(&updated).await;

        assert!(
            okx.is_closed(),
            "a rotated venue origin should close the connection of that venue"
        );
        assert!(!binance.is_closed(), "another venue keeps its connection");
    }

    #[tokio::test]
    async fn invalidate_stale_closes_a_socket_whose_own_role_moved() {
        let route = binance_route();
        let initial = MarketParticipantNetworkSnapshot::new(Config::default());
        let pool = pool();
        let socket = open(
            &pool,
            &initial,
            &route,
            stream_host(&initial, &route)
                .join("/ws")
                .expect("the fixture path joins"),
        )
        .await;
        let rest = open(
            &pool,
            &initial,
            &route,
            request_host(&initial, &route)
                .join("/api/v3/exchangeInfo")
                .expect("the fixture path joins"),
        )
        .await;
        assert!(
            !socket.is_closed(),
            "the fixture socket is open before the venue's stream host moves"
        );
        assert!(!rest.is_closed());

        let mut moved = Config::default();
        assert!(moved.set_binance_market_origins(
            &venue("binance"),
            BinanceMarket::Spot,
            &BinanceOrigins::new(
                BinanceMarket::Spot,
                BinanceHttpOrigin::Api,
                BinanceWebsocketOrigin::StreamAlt,
            ),
        ));
        let updated = MarketParticipantNetworkSnapshot::from_previous(moved, Some(&initial));
        pool.invalidate_stale(&updated).await;

        assert!(
            socket.is_closed(),
            "a WebSocket whose own origin moved is closed"
        );
        assert!(
            !rest.is_closed(),
            "a request connection whose own origin stayed is kept"
        );
    }

    async fn open(
        pool: &ConnectionPool,
        snapshot: &MarketParticipantNetworkSnapshot,
        route: &VenueRoute,
        endpoint: Url,
    ) -> Arc<Connection> {
        pool.get_or_create_fixture(
            ConnectionKey::for_route(
                endpoint,
                ResolvedProxy::NoProxy,
                None,
                route.clone(),
                snapshot.network_route_generation(&route.venue),
                &ConnectionProtocol::new(no_response_id),
            ),
            ConnectionState::stateless(),
        )
        .await
    }

    #[tokio::test]
    async fn invalidate_stale_keeps_an_unhealthy_connection_of_a_venue_that_did_not_change() {
        let mut config = Config::default();
        config.set_network_generations(generations([(venue("binance"), 41)]));
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());
        let pool = pool();
        let binance = fixture_connection(&pool, &initial, &binance_route()).await;
        let okx = fixture_connection(&pool, &initial, &okx_route()).await;
        okx.mark_io_error();
        assert!(
            !okx.is_healthy(),
            "the fixture okx connection is unhealthy before the publication"
        );

        config.set_network_generations(generations([(venue("binance"), 77)]));
        let updated = MarketParticipantNetworkSnapshot::from_previous(config, Some(&initial));
        pool.invalidate_stale(&updated).await;

        assert!(binance.is_closed(), "a changed venue should be closed");
        assert!(
            !okx.is_closed(),
            "an unhealthy connection of an unchanged venue keeps recovering: health is a dial-time judgement, never an invalidation"
        );
    }

    #[tokio::test]
    async fn invalidate_stale_closes_a_changed_venues_unhealthy_connection() {
        let mut config = Config::default();
        config.set_network_generations(generations([(venue("binance"), 41)]));
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());
        let pool = pool();
        let binance = fixture_connection(&pool, &initial, &binance_route()).await;
        let okx = fixture_connection(&pool, &initial, &okx_route()).await;
        binance.mark_io_error();

        config.set_network_generations(generations([(venue("binance"), 77)]));
        let updated = MarketParticipantNetworkSnapshot::from_previous(config, Some(&initial));
        pool.invalidate_stale(&updated).await;

        assert!(
            binance.is_closed(),
            "a changed venue is closed even while its connection is unhealthy"
        );
        assert!(!okx.is_closed(), "another venue keeps its connection");
    }

    #[tokio::test]
    async fn a_subscribed_connection_replaced_by_a_new_dial_is_not_closed() {
        let server = InfoSocket::spawn("[]".to_owned(), Vec::new()).await;
        let pool = pool();
        let endpoint = Url::parse(server.url()).expect("the fixture socket serves a URL");
        let key = ConnectionKey::new(
            endpoint.clone(),
            ResolvedProxy::NoProxy,
            None,
            0,
            &ConnectionProtocol::new(no_response_id),
        );
        let policy = ReconnectPolicy {
            initial_delay: Duration::from_millis(200),
            multiplier: 1,
            max_delay: Duration::from_millis(200),
            max_attempts: Some(3),
        };
        let previous = pool
            .connect_or_create_with_endpoint(
                key.clone(),
                endpoint.clone(),
                ConnectionState::stateless(),
                ConnectionProtocol::new(no_response_id),
                pool.cancellation(),
                policy,
            )
            .await
            .expect("the fixture socket answers the first dial")
            .0;
        let subscription = previous
            .subscribe(
                SubscriptionTopic::from(Bytes::from_static(b"candles:1m")),
                Bytes::from_static(br#"{"op":"subscribe","channel":"candles:1m"}"#),
                Bytes::from_static(br#"{"op":"unsubscribe","channel":"candles:1m"}"#),
            )
            .expect("the first connection accepts a subscription");
        previous.mark_io_error();
        assert!(
            !previous.is_healthy(),
            "the dial below is only reached for a connection the pool will not hand out"
        );

        let (replacement, created) = pool
            .connect_or_create_with_endpoint(
                key.clone(),
                endpoint,
                ConnectionState::stateless(),
                ConnectionProtocol::new(no_response_id),
                pool.cancellation(),
                policy,
            )
            .await
            .expect("the fixture socket answers the second dial");
        assert!(created, "an unhealthy connection is replaced by a new dial");
        let pooled = pool
            .get(&key)
            .await
            .expect("the key is pooled after the dial");
        assert!(
            Arc::ptr_eq(&pooled, &replacement),
            "the new dial is the connection the key is answered from"
        );
        assert!(
            !Arc::ptr_eq(&pooled, &previous),
            "the retired connection is out of the pool"
        );
        assert!(
            !previous.is_closed(),
            "a connection whose subscription is still held is retired rather than closed"
        );

        drop(subscription);
        previous.close();
        replacement.close();
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_retired_stateful_connection_is_collected_when_its_last_subscription_drains() {
        let server = InfoSocket::spawn("[]".to_owned(), Vec::new()).await;
        let pool = pool();
        let endpoint = Url::parse(server.url()).expect("the fixture socket serves a URL");
        let protocol =
            ConnectionProtocol::new(no_response_id).with_idle_timeout(Duration::from_millis(50));
        let key = ConnectionKey::new(endpoint.clone(), ResolvedProxy::NoProxy, None, 0, &protocol);
        let policy = ReconnectPolicy {
            initial_delay: Duration::from_millis(200),
            multiplier: 1,
            max_delay: Duration::from_millis(200),
            max_attempts: Some(3),
        };
        let previous = pool
            .connect_or_create_with_endpoint(
                key.clone(),
                endpoint.clone(),
                ConnectionState::stateful(),
                protocol.clone(),
                pool.cancellation(),
                policy,
            )
            .await
            .expect("the fixture socket answers the first dial")
            .0;
        let subscription = previous
            .subscribe(
                SubscriptionTopic::from(Bytes::from_static(b"orders")),
                Bytes::from_static(br#"{"op":"subscribe","channel":"orders"}"#),
                Bytes::from_static(br#"{"op":"unsubscribe","channel":"orders"}"#),
            )
            .expect("the first connection accepts a subscription");
        previous.mark_io_error();
        let (replacement, created) = pool
            .connect_or_create_with_endpoint(
                key.clone(),
                endpoint,
                ConnectionState::stateful(),
                protocol,
                pool.cancellation(),
                policy,
            )
            .await
            .expect("the fixture socket answers the second dial");
        assert!(created, "an unhealthy connection is replaced by a new dial");
        assert!(
            !previous.is_closed(),
            "the subscription still held is what keeps the retired connection open"
        );

        drop(subscription);

        timeout(Duration::from_secs(5), async {
            while !previous.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the retired connection ends when its last subscription drains");
        replacement.close();
        server.shutdown().await;
    }

    #[tokio::test]
    async fn invalidate_stale_closes_authenticated_connections_when_credentials_change() {
        let mut config = Config::default();
        binance_credentials(&mut config, "api-a", "private-a");
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());
        let credential_generation = initial
            .credential_generation(&venue("binance"), &scope("spot"))
            .expect("configured credentials should have a generation");
        let pool = pool();
        let mut key = fixture_key(&initial, &binance_route());
        key.with_credentials(
            Some(MarketParticipantAccountId::new("account").expect("the account id is valid")),
            Some(credential_generation),
        );
        let connection = pool
            .get_or_create_fixture(key, ConnectionState::stateful())
            .await;

        binance_credentials(&mut config, "api-b", "private-b");
        let updated = MarketParticipantNetworkSnapshot::from_previous(config, Some(&initial));
        pool.invalidate_stale(&updated).await;

        assert!(
            connection.is_closed(),
            "credential changes should close the old authenticated connection"
        );
    }

    #[test]
    fn a_credential_generation_is_per_venue() {
        let mut config = Config::default();
        binance_credentials(&mut config, "key", "private");
        okx_credentials(&mut config, "key", "secret");
        let snapshot = MarketParticipantNetworkSnapshot::new(config);
        let binance = snapshot.credential_generation(&venue("binance"), &scope("spot"));
        let okx = snapshot.credential_generation(&venue("okx"), &scope("spot"));

        assert!(binance.is_some());
        assert!(okx.is_some());
        assert_ne!(binance, okx);
        assert!(
            binance.map(CredentialGeneration::get) < okx.map(CredentialGeneration::get),
            "the first configured venue is allocated before the second"
        );
    }

    #[test]
    fn every_venue_resolves_its_own_configuration() {
        let snapshot = MarketParticipantNetworkSnapshot::new(Config::default());

        for venue in VenueId::ALL {
            assert!(
                snapshot.venue_settings(&venue).is_some(),
                "{venue} is configured by the decoded shape"
            );
            assert!(
                snapshot.origins(&venue, &scope("spot")).is_some(),
                "{venue} resolves an origin for a scope it does not publish, through the pair it deploys with"
            );
            assert!(
                snapshot.origins(&venue, &scope("perp-dex/xyz")).is_some(),
                "{venue} resolves an origin for a scope it deploys"
            );
        }
    }
}
