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

/// Thread-safe registry of reusable market-participant connections.
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

/// The session credential one handshake obtained for the socket it opened.
///
/// The value is opaque to the pool: what a venue authenticates a socket with is the
/// venue's own fact, and the caller that asked for it is the only one that knows what to
/// write from it. It travels beside the reauthentication so a caller does not have to ask
/// the venue twice for the same credential.
pub type SessionCredential = smol_str::SmolStr;

/// What one handshake answers: the reauthentication a new socket replays, and — on the
/// run that dialled it — the credential that socket is authenticated with.
///
/// The second half is `None` for a run that only built the reauthentication, and the
/// credential itself for the run that dialled: the closure is reusable, so the run that
/// dialled is the one that knows what the venue handed out.
pub type Handshaken = (
    crate::connection::Reauthentication,
    Option<SessionCredential>,
);

/// The handshake one dial was given, when it is not a value the caller already holds.
///
/// A venue whose reauthentication is a value the caller holds passes it in
/// [`PoolConnectOptions`]; one whose credential is a request to the venue passes this and
/// has it run inside the pool's own miss.
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
    /// Runs the handshake this dial was given.
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

    /// Returns the pool's entry for one fixture connection, inserting one on a miss.
    ///
    /// A fixture connection dials nothing, so a test that drives what the pool does to a
    /// pooled entry — replacement, invalidation — needs no socket behind it. It is
    /// crate-visible because the root's own tests drive the invalidation task `install`
    /// starts against the pool `install` built, and that pool is not this module's.
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

    /// Returns a healthy WebSocket-backed connection, establishing it on a miss.
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

    /// Returns a healthy connection, dialing one through `handshake` on a miss.
    ///
    /// A venue whose dial needs a session credential it has to ask the venue for — a
    /// one-time password, a ticket — must not ask for it when the socket it would use is
    /// already pooled: the refusal that costs nothing is the one that is never sent, and a
    /// credential asked for a socket nothing dialled is a request the venue counts. The
    /// closure therefore runs inside the miss, once the pool has decided that no healthy
    /// connection answers the key, and what it answers is the reauthentication the new
    /// connection's own socket replays before the connection is handed back — so a caller
    /// that dials this way never installs one afterwards — together with the credential
    /// itself, for the state that reauthentication establishes.
    ///
    /// A connection the pool already held answers `None` for that credential: nothing was
    /// asked of the venue, and what a socket it already authenticated is authenticated
    /// with is what its own state says.
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
        // A second dial of the same key is the same connection, so the one that lost the
        // race is closed and the stored one is handed out. A dial of a *different* key is
        // not here at all: the upgrade headers a socket presents are read into the key
        // from the protocol that dials it, so a socket dialled for one gateway can only
        // be found under the key that gateway was declared on — a request needing another
        // declaration builds another entry and was inserted under its own key.
        if let Some(existing) = connections.get(&key).cloned()
            && existing.is_healthy()
        {
            connection.close();
            return Ok((existing, false, None));
        }
        // A previous connection that still has subscribers is retired rather than
        // closed: its subscriptions outlive the socket they were dialled on, so ending
        // them here would end a live stream for a socket that was never the problem.
        // Retired this way it is out of the pool, and a later snapshot can no longer
        // close it directly; it ends by its own lifecycle instead — its last
        // subscription draining starts the idle clock the actor collects it on, and a
        // reconnect that cannot come back closes it. One that has no subscribers is the
        // socket nobody is reading any more, and is closed as it always was.
        if let Some(previous) = connections.insert(key, connection.clone()) {
            if previous.has_subscriptions() {
                previous.retire();
            } else {
                previous.close();
            }
        }
        Ok((connection, true, credential))
    }

    /// Returns the connection a key is answered by, if the pool holds one.
    ///
    /// Health is deliberately not part of the answer. It is the same dial-time judgement
    /// [`Self::invalidate_stale`] refuses to close on, asked at the other end: a
    /// connection that is mid-reconnect is still the one its key is answered by, so asking
    /// here would answer nothing for a key the pool holds. A caller that may not use such a
    /// connection asks for itself, which is what the adapter reusing a pooled socket does
    /// (`pool.get(&key).await.filter(|value| value.is_healthy())`, before it registers a
    /// listen key) and what the Longbridge session asks before it reuses one.
    pub async fn get(&self, key: &ConnectionKey) -> Option<Arc<Connection>> {
        self.connections.read().await.get(key).cloned()
    }

    /// Closes every connection the pool holds.
    ///
    /// This is the shutdown boundary rather than a sweep of everything this pool ever
    /// dialled: a connection retired by a replacement is out of the table — that is what
    /// retiring means — and is not reached from here. It ends by its own lifecycle instead
    /// (its idle clock once its last subscription drains, or a reconnect that cannot come
    /// back), which is the boundary the user accepted in exchange for a replacement never
    /// ending a live stream. A process that is going away drops it with everything else.
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

    /// Closes every connection the new snapshot no longer serves.
    ///
    /// A key is stale when any dimension it was built with no longer matches the
    /// snapshot: the proxy decision of its own venue, the origin that venue's
    /// settings give its scope now, or the credential generation of that scope —
    /// each read back by the same venue and scope the key names, so no connection
    /// survives on a venue-level guess, and no connection dies on another venue's.
    ///
    /// Health is deliberately not a fourth dimension. It is a dial-time judgement,
    /// made where a connection is handed out — `connect_or_create_*` refuses an
    /// unhealthy connection and dials a new one, and a venue with a session of its
    /// own asks the same question before it reuses one — not an invalidation: a
    /// connection that is mid-reconnect has lost its socket and not its key, and
    /// [`Connection::close`] is destructive. It takes the publisher every bound
    /// subscription ends on and cancels the actor, so an unhealthy connection
    /// closed here does not reconnect; its streams end instead of being replayed.
    /// A configuration write for one venue would therefore tear down the
    /// recovering sockets of every other venue, which is exactly what a snapshot
    /// of one venue's settings must not do.
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
            fixture::{self, FixtureAddress},
            origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
            settings::{BinanceOrigins, VenueSettings},
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

    /// The Binance spot route every fixture connection belongs to.
    fn binance_route() -> VenueRoute {
        route("binance", "spot")
    }

    /// The route of a second venue, for the checks that need one connection per
    /// venue.
    fn okx_route() -> VenueRoute {
        route("okx", "spot")
    }

    /// The stream host one route's own configuration gives its scope.
    fn stream_host(snapshot: &MarketParticipantNetworkSnapshot, route: &VenueRoute) -> Url {
        snapshot
            .origins(&route.venue, &route.scope)
            .expect("a built-in venue is configured")
            .websocket
    }

    /// The request host one route's own configuration gives its scope.
    fn request_host(snapshot: &MarketParticipantNetworkSnapshot, route: &VenueRoute) -> Url {
        snapshot
            .origins(&route.venue, &route.scope)
            .expect("a built-in venue is configured")
            .http
    }

    /// The WebSocket origin one route's venue configuration gives its scope.
    ///
    /// A connection key carries the origin it was dialled on, so a fixture key has
    /// to dial the origin its venue resolves or the pool reads it as stale.
    fn configured_origin(snapshot: &MarketParticipantNetworkSnapshot, route: &VenueRoute) -> Url {
        snapshot
            .origins(&route.venue, &route.scope)
            .expect("a built-in venue is configured")
            .websocket
    }

    /// The key of one fixture connection of `route`.
    fn fixture_key(
        snapshot: &MarketParticipantNetworkSnapshot,
        route: &VenueRoute,
    ) -> ConnectionKey {
        fixture_key_declaring(snapshot, route, UpgradeHeaders::default())
    }

    /// The key of one fixture connection whose dial declares these upgrade headers.
    ///
    /// A fixture key is built from the protocol its dial would use, so the declaration
    /// this names is the one the dial presents.
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

    /// A pool whose transport reaches a loopback fixture.
    ///
    /// Most of the fixtures below never dial — a fixture connection is built by
    /// `get_or_create_fixture` — but the ones that hand a dial to the pool have to
    /// reach the socket the test itself serves, and plaintext is denied by default.
    fn pool() -> ConnectionPool {
        ConnectionPool::with_transport(Arc::new(TransportHub::new(plaintext_policy())))
    }

    /// One venue's proxy-decision generations, as a commit stamps them.
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
        let Some(VenueSettings::Binance(settings)) = config.venues.get_mut(&venue("binance"))
        else {
            panic!("binance is configured as Binance by the built-in table");
        };
        settings.credentials.spot_api_key = api_key.into();
        settings.credentials.spot_private_key = private_key.into();
    }

    fn okx_credentials(config: &mut Config, api_key: &str, secret_key: &str) {
        let Some(VenueSettings::Okx(settings)) = config.venues.get_mut(&venue("okx")) else {
            panic!("okx is configured as OKX by the built-in table");
        };
        settings.credentials.api_key = api_key.into();
        settings.credentials.secret_key = secret_key.into();
        settings.credentials.passphrase = "pass".into();
    }

    /// One fixture address a venue's published origins resolve at, for both roles.
    fn fixture_address(port: u16) -> FixtureAddress {
        let address =
            Url::parse(&format!("http://127.0.0.1:{port}")).expect("the fixture address is a URL");
        FixtureAddress::at(&address)
    }

    /// Opens the fixture connection of one route.
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

    /// A venue that routes its sockets by an upgrade header is served from the gateway
    /// that header names, so the pool has to hold one connection per declaration: a
    /// second request declaring another region must not be handed the first socket.
    ///
    /// The keys are also checked directly, because the pool's own entry is decided by
    /// them: two keys that differ only in their declaration are two entries, whatever the
    /// pool does with either. Each key is built from the protocol its own dial would
    /// carry, so the entry a request is served from is the one its declaration names.
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

    /// The declaration a fixture connection dials with.
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

        // The Binance venue's proxy decision moves; the OKX venue's does not.
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

        // Only the OKX venue moves to another address.
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

    /// A connection is served by the origin of its own role, and only by it.
    ///
    /// A venue whose request host stays where it is and whose stream host moves must
    /// invalidate the stream and keep the request connection: the socket is still
    /// reading from the host the user replaced, whatever the HTTP field happens to
    /// hold, and a check that accepted either field would answer "still served" for
    /// exactly this reconfiguration — which is what this pins. Both choices are ones
    /// the venue publishes, so the move is one this build can make.
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

        // The stream host moves to the venue's other published stream and the request
        // host keeps the one it names, so only the socket's own role moved.
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

    /// Opens one fixture connection dialled on `endpoint` for `route`.
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

    /// A read-only socket that is mid-reconnect is not what a configuration write is
    /// about, so invalidation leaves it alone: its key did not move, and closing it here
    /// would end the streams waiting for it to come back instead of letting its own
    /// reconnect answer them.
    #[tokio::test]
    async fn invalidate_stale_keeps_an_unhealthy_connection_of_a_venue_that_did_not_change() {
        let mut config = Config::default();
        config.set_network_generations(generations([(venue("binance"), 41)]));
        let initial = MarketParticipantNetworkSnapshot::new(config.clone());
        let pool = pool();
        let binance = fixture_connection(&pool, &initial, &binance_route()).await;
        let okx = fixture_connection(&pool, &initial, &okx_route()).await;
        // The socket of the venue the publication does not touch is between sockets, the
        // way it is while it backs off and re-authenticates.
        okx.mark_io_error();
        assert!(
            !okx.is_healthy(),
            "the fixture okx connection is unhealthy before the publication"
        );

        // The Binance venue's proxy decision moves; the OKX venue's does not.
        config.set_network_generations(generations([(venue("binance"), 77)]));
        let updated = MarketParticipantNetworkSnapshot::from_previous(config, Some(&initial));
        pool.invalidate_stale(&updated).await;

        assert!(binance.is_closed(), "a changed venue should be closed");
        assert!(
            !okx.is_closed(),
            "an unhealthy connection of an unchanged venue keeps recovering: health is a dial-time judgement, never an invalidation"
        );
    }

    /// Health is not what invalidation closes on, so a key that did move is closed whether
    /// the connection behind it is healthy or not: an unhealthy connection whose own
    /// venue was reconfigured is one the next dial replaces, and leaving it pooled would
    /// have the pool keep answering the stale key with it.
    ///
    /// The publication is the same one the test above publishes, and the connection it
    /// ends is the one of the *changed* venue — that one mark is what separates the two
    /// expectations, so this pins that the removal above took the health clause with it
    /// rather than the whole close.
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

    /// A connection that is replaced in the pool while a subscriber is reading it stays
    /// open: its subscription outlives the socket it was dialled on, so the replacement
    /// retires it instead of ending the stream under the subscriber.
    ///
    /// The retired connection is then out of the pool and no longer reachable by
    /// invalidation; it ends by its own lifecycle — idle collection once its last
    /// subscription drains, or a reconnect policy that gives up. That end is not
    /// asserted here: the idle window is a minute and the fixture's socket actor is the
    /// one that would collect it, so what a unit fixture can pin is the state the pool
    /// leaves behind, which is what the assertions below read.
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
        // The lease is held for the rest of the test: a connection with a subscriber is
        // the one the replacement has to keep alive.
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

    /// What ends a retired connection is its own lifecycle, and a stateful one does not
    /// reach idle collection the way a stateless one does: it is kept for the requests
    /// its credentials serve, and a connection the pool has retired is never asked for
    /// one. Its last subscription draining is therefore its own end, as the replacement
    /// path promises — otherwise the socket, its actor and its listen-key refresh live
    /// for the life of the process.
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
        // The dial below is the one that retires this connection, and it is only reached
        // for one the pool will not hand out.
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

    /// A credential generation belongs to the venue whose credentials it tracks, so
    /// two configured venues never share one.
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

    /// Every venue this build speaks has a row and resolves an origin for every scope a
    /// request may address, so no request is ever dialled on settings nothing wrote.
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
