use app_config::network::{NetworkRouteGeneration, ResolvedProxy};
#[cfg(test)]
use market_participants_model::identifier::{VenueId, VenueScopeId};
use market_participants_model::{
    identifier::MarketParticipantAccountId, listing::route::VenueRoute,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use smol_str::SmolStr;
use url::Url;

use crate::{connection::physical::ConnectionProtocol, routing::CredentialGeneration};

/// The headers one socket's WebSocket upgrade request has to carry, in canonical form.
///
/// A venue may decide where to serve a socket from a request header — Longbridge routes
/// its quote socket by `x-dc-region`, which its own credentials determine — and such a
/// header is a transport fact: a socket that presented one value is not the socket a
/// request needing another value may be answered from. This is that fact in the form
/// [`ConnectionKey`] can compare, so the declaration is a dimension of the key rather
/// than a detail of the dial that happened to create it.
///
/// It is a derived form of `http::HeaderMap`, which is `PartialEq` but not `Hash`: the
/// pairs are `(name, value)` sorted, which makes equality and hashing order-independent
/// without the map. The pairs are the *materialisable* ones — a header is part of this
/// form only while it can be rebuilt as a valid `HeaderName`/`HeaderValue` pair, and
/// [`UpgradeHeaders::to_header_map`] rebuilds exactly what was counted here. A value no
/// later dial could present is therefore not a declaration this type ever holds, rather
/// than one it drops on the way out.
///
/// [`Debug`] names the headers and redacts their values: a header can carry a token, so
/// rendering one into a log line is not this type's decision to make, while *which*
/// headers a key carries is what an operator comparing two keys needs to see.
///
/// The empty declaration is the one a venue that does not route its sockets by a request
/// header dials with, which is why the [`Default`] is the empty form rather than an
/// absence: every connection has a declaration, and for most venues it is nothing.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct UpgradeHeaders(Vec<(SmolStr, SmolStr)>);

/// What [`UpgradeHeaders`] renders in place of the header values.
///
/// It is one constant because two of them — the `Debug` impl and the test that pins the
/// redaction — would otherwise be free to drift apart.
pub(crate) const REDACTED_UPGRADE_HEADER_VALUES: &str = "<redacted>";

impl UpgradeHeaders {
    /// Reads one protocol's declared headers into the canonical form.
    ///
    /// The declaration is which headers the socket presents, never what some other map
    /// holds: an entry that is not both a `HeaderName` and a `HeaderValue` is not one this
    /// stack could put on an upgrade request at all, so it never becomes a dimension —
    /// rather than being counted by the key and then dropped by the dial.
    /// `HeaderName::from_bytes` folds a name to its canonical lowercase form, and
    /// `HeaderValue::from_str` accepts the bytes an HTTP request may carry (visible ASCII,
    /// with the whitespace around a value trimmed) and nothing else: a control byte or a
    /// non-ASCII one is left out of the declaration here, and what a `HeaderMap` can hold
    /// as opaque bytes while no dial could present it as text is left out too.
    ///
    /// An entry this drops is dropped on purpose and not reported: a caller declares the
    /// headers its venue requires on a socket, and one that cannot be dialled could not be
    /// presented whatever this did with it — the key and the dial agree, which is the
    /// property that matters, and neither of them carries a header the other does not.
    pub fn from_header_map(headers: &HeaderMap) -> Self {
        let mut pairs: Vec<(SmolStr, SmolStr)> = Vec::with_capacity(headers.len());
        pairs.extend(headers.iter().filter_map(|(name, value)| {
            let name = HeaderName::from_bytes(name.as_str().as_bytes()).ok()?;
            let value = HeaderValue::from_str(value.to_str().ok()?).ok()?;
            Some((
                SmolStr::new(name.as_str()),
                SmolStr::new(value.to_str().ok()?),
            ))
        }));
        // `HeaderMap` iterates in an unspecified order, and two maps that declare the same
        // headers have to canonicalize to equal keys whatever that order was.
        pairs.sort_unstable();
        Self(pairs)
    }

    /// Materialises the header map a dial presents, from the canonical form alone.
    ///
    /// This is the only way a declaration reaches a WebSocket upgrade request, so what
    /// the key compares and what the socket carries cannot be two values: every pair here
    /// was accepted as a `HeaderName`/`HeaderValue` when it was read in, and these are
    /// the pairs read back out. Two declarations that differ are two maps, because the
    /// canonical form is order-independent and materialising it is deterministic.
    ///
    /// The map is built fresh rather than kept: it is the request's, while the
    /// declaration outlives any single dial — the connection re-presents it on every
    /// reconnect. It is public because it is the other half of
    /// `ConnectionProtocol::with_upgrade_headers`: a caller that declares headers on a
    /// protocol can read back exactly what every dial of that connection will present.
    pub fn to_header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::with_capacity(self.0.len());
        for (name, value) in &self.0 {
            let name = HeaderName::from_bytes(name.as_bytes())
                .expect("a name read as a header name is a header name");
            let value = HeaderValue::from_str(value)
                .expect("a value read as a header value is a header value");
            // A name declared twice keeps both values in the map, exactly as the
            // canonical form holds both pairs.
            headers.append(name, value);
        }
        headers
    }
}

impl std::fmt::Debug for UpgradeHeaders {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = Vec::with_capacity(self.0.len());
        names.extend(self.0.iter().map(|(name, _)| name.as_str()));
        let names = names.join(", ");
        formatter
            .debug_tuple("UpgradeHeaders")
            .field(&names)
            .field(&REDACTED_UPGRADE_HEADER_VALUES)
            .finish()
    }
}

/// All transport dimensions that determine whether a physical socket is reusable.
///
/// The route is a required dimension, not an optional annotation: two scopes that
/// share one origin — Hyperliquid core, a HIP-3 DEX and a HIP-4 outcome venue, for
/// instance — must never be served by one socket unless the venue says their
/// traffic belongs together, and a connection whose route is not part of the key
/// could not be invalidated when that scope changes. Hyperliquid says exactly that
/// for wallet monitoring: every deployed scope of one monitored address is read
/// through one socket, so those connections are keyed by the canonical wallet scope
/// rather than by the scope that opened them.
///
/// Folding the route does not fold the transport: the origin and proxy of a key
/// stay the ones the request was resolved with, so two scopes that resolve
/// different transport still dial separate sockets. The route generation is the one
/// exception — a folded connection carries the generation of the canonical route
/// its key names, because that is the route the pool re-reads when it decides
/// whether the connection is still current.
///
/// The upgrade headers are a dimension for the same reason the origin is. A venue that
/// routes its sockets by a request header is served from the gateway that header names,
/// and the header can follow something the key does not otherwise carry — credentials,
/// which a socket's own account and credential generation describe but a scope's
/// region does not. A socket dialled for one value of it would answer requests that
/// need another value as if it had been dialled for them, and every reconnect would
/// re-dial the same wrong gateway, so two declarations that differ have to be two keys.
///
/// The comparison is over the declarations that can be dialled, which is what
/// [`UpgradeHeaders`] holds: two maps differing only in a value no dial could present —
/// a name that is not a `HeaderName`, or a value carrying a control character or a byte
/// outside ASCII — are not two gateways and are one key. That exclusion is decided where
/// the declaration is read (see [`UpgradeHeaders::from_header_map`]), not when the dial
/// materialises it, so the key never counts a header the socket does not carry.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ConnectionKey {
    /// The origin that was dialled.
    pub endpoint: Url,
    /// The proxy hop the socket was opened through.
    pub proxy: ResolvedProxy,
    /// The authentication identity, absent for public connections.
    pub auth: Option<SmolStr>,
    /// The account this connection is authenticated as, for authenticated sockets.
    pub account: Option<MarketParticipantAccountId>,
    /// The venue route this connection belongs to.
    pub route: VenueRoute,
    /// Generation of the network/proxy route used for this connection.
    pub network_route_generation: NetworkRouteGeneration,
    /// Generation of the venue's credentials bound to authenticated connections.
    pub credential_generation: Option<CredentialGeneration>,
    /// The headers every dial of this connection has to present on its upgrade request.
    pub upgrade_headers: UpgradeHeaders,
}

impl std::fmt::Debug for ConnectionKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionKey")
            .field("endpoint", &self.redacted_endpoint())
            .field("proxy", &self.proxy)
            .field("auth", &"<redacted>")
            .field("account", &self.account)
            .field("route", &self.route)
            .field("network_route_generation", &self.network_route_generation)
            .field("credential_generation", &self.credential_generation)
            .field("upgrade_headers", &self.upgrade_headers)
            .finish()
    }
}

impl ConnectionKey {
    /// Redacts the path and query of the dialled origin.
    ///
    /// A listen key or an API key can travel in the path, so the rendered key
    /// keeps the origin only.
    pub(crate) fn redacted_endpoint(&self) -> Url {
        let mut endpoint = self.endpoint.clone();
        endpoint.set_path("/");
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        endpoint
    }

    /// Builds the key of a test fixture connection.
    ///
    /// A connection key always carries a route, so a fixture that dials nothing
    /// this build serves names the fixture route: what such a test exercises is
    /// the transport, and the route is a dimension of the key rather than its
    /// subject. The protocol is the fixture's own, because the declaration is the
    /// protocol's: a fixture key and the dial it stands for carry one declaration
    /// for the same reason a venue's key and dial do.
    #[cfg(test)]
    pub fn new(
        endpoint: Url,
        proxy: ResolvedProxy,
        auth: Option<SmolStr>,
        network_route_generation: u64,
        protocol: &ConnectionProtocol,
    ) -> Self {
        Self::for_route(
            endpoint,
            proxy,
            auth,
            VenueRoute::new(
                VenueId::Binance,
                VenueScopeId::new("fixture").expect("the fixture scope is canonical"),
            ),
            NetworkRouteGeneration::new(network_route_generation),
            protocol,
        )
    }

    /// Builds the key of one venue connection.
    ///
    /// The upgrade headers are read off the protocol rather than taken as a second
    /// argument: they are one value, declared where the venue declares how it speaks
    /// and carried by the key that decides whether a socket may be reused, so no
    /// caller is in a position to hand the key a declaration other than the one the
    /// dial presents. A key built from a protocol without one carries none, which is
    /// every venue that does not route its sockets by a request header.
    pub fn for_route(
        endpoint: Url,
        proxy: ResolvedProxy,
        auth: Option<SmolStr>,
        route: VenueRoute,
        network_route_generation: NetworkRouteGeneration,
        protocol: &ConnectionProtocol,
    ) -> Self {
        Self {
            endpoint,
            proxy,
            auth,
            account: None,
            route,
            network_route_generation,
            credential_generation: None,
            upgrade_headers: protocol.upgrade_headers.clone(),
        }
    }

    /// Binds the credential identity of one connection.
    ///
    /// The venue is the credential identity and is already part of the key, so a
    /// venue connection binds the account and the credential generation it was
    /// given rather than guessing either from the account.
    pub(crate) fn with_credentials(
        &mut self,
        account: Option<MarketParticipantAccountId>,
        generation: Option<CredentialGeneration>,
    ) {
        self.account = account;
        self.credential_generation = generation;
    }
}

#[cfg(test)]
mod tests {
    use ahash::AHashSet;
    use app_config::network::ResolvedProxy;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    use url::Url;

    use super::{ConnectionKey, REDACTED_UPGRADE_HEADER_VALUES, UpgradeHeaders};
    use crate::{
        connection::physical::{ConnectionProtocol, no_response_id},
        routing::CredentialGeneration,
    };

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new(scope).expect("the scope should be canonical"),
        )
    }

    /// Builds the header map a venue declares.
    ///
    /// The value is parsed as a header value rather than as a string, which is the
    /// lenient of the two: a value the HTTP stack accepts as opaque bytes but cannot read
    /// back as text can be declared here and still is not one an upgrade request may
    /// carry, which is the case [`UpgradeHeaders::from_header_map`] has to decide.
    fn declared(headers: &[(&str, &str)]) -> HeaderMap {
        let mut declared = HeaderMap::new();
        for (name, value) in headers {
            declared.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("the header name should be valid"),
                HeaderValue::from_bytes(value.as_bytes())
                    .expect("the header value should be valid"),
            );
        }
        declared
    }

    /// The declaration a venue's protocol carries.
    fn upgrade_headers(headers: &[(&str, &str)]) -> UpgradeHeaders {
        UpgradeHeaders::from_header_map(&declared(headers))
    }

    fn key(route: VenueRoute, endpoint: &str) -> ConnectionKey {
        key_with_headers(route, endpoint, &[])
    }

    /// The key of one fixture connection whose dial declares `headers`.
    ///
    /// The declaration is not a parameter of the key: it is the protocol's, and reading
    /// it there is what makes the key and the dial one value. This declares the headers
    /// on the protocol a fixture dials with and builds the key from that protocol, so a
    /// fixture cannot state one declaration for its key and another for its dial.
    fn key_with_headers(
        route: VenueRoute,
        endpoint: &str,
        headers: &[(&str, &str)],
    ) -> ConnectionKey {
        let declaration =
            ConnectionProtocol::new(no_response_id).with_upgrade_headers(upgrade_headers(headers));
        ConnectionKey::for_route(
            Url::parse(endpoint).expect("the test endpoint should parse"),
            ResolvedProxy::NoProxy,
            Some("api-secret".into()),
            route,
            app_config::network::NetworkRouteGeneration::new(1),
            &declaration,
        )
    }

    #[test]
    fn debug_redacts_endpoint_path_and_query() {
        let key = key(
            route("core/spot"),
            "wss://example.test/ws/listen-key-secret?apiKey=api-secret",
        );
        let debug = format!("{key:?}");
        assert!(!debug.contains("listen-key-secret"));
        assert!(!debug.contains("api-secret"));
    }

    /// The upgrade headers are compared by what they declare, not by how the map that
    /// declared them happened to be built: the order two headers were inserted in is not
    /// part of the connection.
    #[test]
    fn the_declared_upgrade_headers_are_a_canonical_form_of_the_map() {
        let route = route("core/spot");
        let declared = key_with_headers(
            route.clone(),
            "wss://example.test/ws",
            &[("x-dc-region", "us"), ("accept-language", "en")],
        );
        let reversed = key_with_headers(
            route,
            "wss://example.test/ws",
            &[("accept-language", "en"), ("x-dc-region", "us")],
        );

        assert_eq!(
            declared, reversed,
            "one declaration inserted in another order is one connection"
        );
    }

    /// The declaration a key compares is the one its protocol dials with, and it is one
    /// value rather than two that have to be kept in step.
    ///
    /// Both halves are asserted at the seam a second declaration would have to come
    /// through: the key is built from the protocol alone — there is no parameter left to
    /// hand it a map of its own — and what the dial presents is the protocol's own
    /// declaration materialised, not a map a caller passed alongside it. A key that took
    /// the headers from anywhere but the protocol fails the second assertion, which is
    /// the state the pool would go on to serve from the wrong gateway.
    #[test]
    fn the_keys_declaration_is_the_one_its_protocol_dials_with() {
        let declaration =
            ConnectionProtocol::new(no_response_id).with_upgrade_headers(upgrade_headers(&[
                ("accept-language", "en"),
                ("x-dc-region", "us"),
            ]));
        let key = ConnectionKey::for_route(
            Url::parse("wss://example.test/ws").expect("the test endpoint should parse"),
            ResolvedProxy::NoProxy,
            None,
            route("core/spot"),
            app_config::network::NetworkRouteGeneration::new(1),
            &declaration,
        );

        assert_eq!(
            UpgradeHeaders::from_header_map(&declaration.upgrade_headers.to_header_map()),
            key.upgrade_headers,
            "the key counts exactly the headers the dial presents"
        );
        assert_ne!(
            key.upgrade_headers,
            upgrade_headers(&[]),
            "a key built from a protocol that declared nothing is not this key, so the \
             assertion above is not met by an empty dimension"
        );
        assert_eq!(
            declaration.upgrade_headers.to_header_map(),
            declared(&[("accept-language", "en"), ("x-dc-region", "us")]),
            "the declaration reaches the dial as the map it was declared as"
        );
    }

    /// A venue that holds its headers as a map declares the same declaration with it.
    ///
    /// The map-shaped seam is the one an adapter reaches for, since a `HeaderMap` is what
    /// it also puts on its HTTP requests, and it has to be the map-shaped spelling of the
    /// same canonical declaration: a second conversion that could disagree with
    /// [`UpgradeHeaders::from_header_map`] would be the two declarations this whole seam
    /// exists to collapse into one.
    #[test]
    fn a_declaration_made_from_a_map_is_the_canonical_one() {
        let headers = declared(&[("accept-language", "en"), ("x-dc-region", "us")]);
        let declaration = ConnectionProtocol::new(no_response_id).with_upgrade_header_map(&headers);

        assert_eq!(
            declaration.upgrade_headers,
            upgrade_headers(&[("accept-language", "en"), ("x-dc-region", "us")]),
            "the map a venue declares with is read into the one canonical form"
        );
        assert_eq!(
            declaration.upgrade_headers.to_header_map(),
            headers,
            "and that form is what every dial of the connection presents"
        );
    }

    /// Only declarations a dial could present are dimensions of a key.
    ///
    /// A header value this stack cannot read back as text — opaque bytes a `HeaderValue`
    /// will hold but an upgrade request cannot put on the wire — is not a header the
    /// socket presents, so a key that counted it would compare a gateway that does not
    /// exist: two such maps would be two connections for one socket. The exclusion is made
    /// once, where the declaration is read, and this pins both ends of it: such a value is
    /// not a dimension, while a header of the same map that *could* be dialled still is.
    #[test]
    fn a_value_no_dial_could_present_is_not_a_key_dimension() {
        let route = route("core/spot");
        // A value the HTTP stack will hold as bytes — `HeaderValue` accepts a byte above
        // ASCII — but that no request can present as text, because reading it back is what
        // fails.
        let unreadable = "us\u{e9}";
        assert!(
            HeaderValue::from_bytes(unreadable.as_bytes()).is_ok(),
            "the fixture value has to be one a header map holds, or the test proves nothing"
        );

        assert_eq!(
            key_with_headers(
                route.clone(),
                "wss://example.test/ws",
                &[("x-dc-region", unreadable)]
            ),
            key(route.clone(), "wss://example.test/ws"),
            "a value no dial could present is not part of the key"
        );
        assert_eq!(
            key_with_headers(
                route.clone(),
                "wss://example.test/ws",
                &[("x-dc-region", unreadable), ("accept-language", "en")]
            ),
            key_with_headers(
                route.clone(),
                "wss://example.test/ws",
                &[("accept-language", "en")]
            ),
            "a key counts the headers a dial could present and no others"
        );
        assert_ne!(
            key_with_headers(
                route.clone(),
                "wss://example.test/ws",
                &[("x-dc-region", "us")]
            ),
            key(route, "wss://example.test/ws"),
            "a header a dial could present is a dimension of the key it is declared in"
        );
    }

    /// A venue that routes its sockets by a request header is served from the gateway
    /// that header names, so two sockets that must present different headers are two
    /// connections: neither the keys nor the pool entries they become may be shared.
    ///
    /// The header this pins is the one the seam exists for — `x-dc-region`, which
    /// Longbridge derives from its credentials — so a key that ignored it would answer
    /// a request declaring `us` from the socket dialled for `ap`, and every reconnect
    /// would re-dial `ap` again. The hash is checked with the equality because a key
    /// that hashes alike is a key that pools alike.
    #[test]
    fn the_upgrade_headers_are_part_of_the_key() {
        let route = route("core/spot");
        let asia = key_with_headers(
            route.clone(),
            "wss://example.test/ws",
            &[("x-dc-region", "ap")],
        );
        let united_states = key_with_headers(
            route.clone(),
            "wss://example.test/ws",
            &[("x-dc-region", "us")],
        );
        let none = key(route, "wss://example.test/ws");

        assert_ne!(asia, united_states);
        assert_ne!(asia, none);
        assert_ne!(united_states, none);

        let mut pooled = AHashSet::with_capacity(3);
        pooled.insert(asia.clone());
        pooled.insert(united_states.clone());
        pooled.insert(none);
        assert_eq!(
            pooled.len(),
            3,
            "two keys that declare different upgrade headers are two pool entries, not one"
        );
        assert!(pooled.contains(&asia) && pooled.contains(&united_states));
    }

    /// The redirecting values a venue may declare are not rendered into a key: a header
    /// can carry a token, so the debug form names the headers and keeps their values out.
    #[test]
    fn debug_redacts_the_upgrade_header_values() {
        let key = key_with_headers(
            route("core/spot"),
            "wss://example.test/ws",
            &[
                ("x-dc-region", "us"),
                ("authorization", "Bearer token-secret"),
            ],
        );

        let debug = format!("{key:?}");

        assert!(!debug.contains("token-secret"));
        assert!(!debug.contains("Bearer"));
        assert!(
            debug.contains(REDACTED_UPGRADE_HEADER_VALUES),
            "the debug form says the values are there and withheld: {debug}"
        );
        assert!(
            debug.contains("x-dc-region"),
            "the headers a key carries are what an operator compares: {debug}"
        );
    }

    /// Market data of two scopes of one origin is never served by one socket.
    ///
    /// Hyperliquid folds its wallet monitor deliberately (see
    /// `shared_socket_route`); nothing else is folded, so the scope still decides
    /// the connection of every other capability.
    #[test]
    fn two_scopes_of_one_origin_are_different_connections() {
        let spot = key(route("core-spot"), "wss://api.hyperliquid.xyz/ws");
        let perp = key(route("core-perp"), "wss://api.hyperliquid.xyz/ws");
        let dex = key(route("perp-dex/xyz"), "wss://api.hyperliquid.xyz/ws");

        assert_ne!(spot, perp);
        assert_ne!(perp, dex);
        assert_ne!(spot, dex);
    }

    #[test]
    fn the_credential_generation_is_part_of_the_key() {
        let account =
            market_participants_model::identifier::MarketParticipantAccountId::new("account")
                .expect("the account id is valid");
        let mut base = key(route("spot"), "wss://example.test/ws");
        base.with_credentials(Some(account.clone()), Some(CredentialGeneration::new(1)));
        let mut rotated = base.clone();
        rotated.with_credentials(Some(account), Some(CredentialGeneration::new(2)));

        assert_ne!(base, rotated);
        assert_eq!(base.route.scope.as_str(), "spot");
    }
}
