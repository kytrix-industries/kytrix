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

#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct UpgradeHeaders(Vec<(SmolStr, SmolStr)>);

pub(crate) const REDACTED_UPGRADE_HEADER_VALUES: &str = "<redacted>";

impl UpgradeHeaders {
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
        pairs.sort_unstable();
        Self(pairs)
    }

    pub fn to_header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::with_capacity(self.0.len());
        for (name, value) in &self.0 {
            let name = HeaderName::from_bytes(name.as_bytes())
                .expect("a name read as a header name is a header name");
            let value = HeaderValue::from_str(value)
                .expect("a value read as a header value is a header value");
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

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ConnectionKey {
    pub endpoint: Url,
    pub proxy: ResolvedProxy,
    pub auth: Option<SmolStr>,
    pub account: Option<MarketParticipantAccountId>,
    pub route: VenueRoute,
    pub network_route_generation: NetworkRouteGeneration,
    pub credential_generation: Option<CredentialGeneration>,
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
    pub(crate) fn redacted_endpoint(&self) -> Url {
        let mut endpoint = self.endpoint.clone();
        endpoint.set_path("/");
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        endpoint
    }

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

    fn upgrade_headers(headers: &[(&str, &str)]) -> UpgradeHeaders {
        UpgradeHeaders::from_header_map(&declared(headers))
    }

    fn key(route: VenueRoute, endpoint: &str) -> ConnectionKey {
        key_with_headers(route, endpoint, &[])
    }

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

    #[test]
    fn a_value_no_dial_could_present_is_not_a_key_dimension() {
        let route = route("core/spot");
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
