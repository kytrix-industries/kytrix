//! Binding one execution's own identity onto the connection key it opens.
//!
//! A key built by [`ConnectionKey::for_route`] carries the origin, the venue route
//! and the generations, but not the account the socket authenticates as. The
//! account is not a detail: the reauthentication handshake reads it back off the
//! key, so a socket whose key has no account can never complete a re-authentication
//! and every authenticated subscription on it fails. The credential generation is
//! bound with it, because the credentials are part of what a socket is: two
//! generations that share an origin are two credential sets and must never be
//! served by one connection.
//!
//! Both values come from the execution the request was validated against — the
//! account from the resolved authentication, the generation from the venue's
//! credentials — and neither is guessed. The credential identity itself is the
//! venue, which the key already names.
//!
//! [`ConnectionKey::for_route`]: crate::connection::ConnectionKey::for_route

use market_participants_model::identifier::MarketParticipantAccountId;

use crate::{
    adapter::ExecutionContext,
    connection::ConnectionKey,
    error::{AuthError, MarketParticipantError},
};

/// Binds the account and credential generation of one execution.
pub(crate) fn bind_execution_identity(
    mut key: ConnectionKey,
    context: &ExecutionContext,
) -> Result<ConnectionKey, MarketParticipantError> {
    let Some(auth) = context.auth.as_ref() else {
        return Ok(key);
    };
    let account = MarketParticipantAccountId::new(auth.account_id()).map_err(|_| {
        MarketParticipantError::AuthenticationFailed {
            route: context.route(),
            account: Some(smol_str::SmolStr::new(auth.account_id())),
            source: AuthError::InvalidAccountIdentity {
                value: smol_str::SmolStr::new(auth.account_id()),
            },
        }
    })?;
    key.with_credentials(Some(account), context.credential_generation());
    Ok(key)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use app_config::{
        market_participants::origins::OriginPair,
        network::{NetworkRouteGeneration, ResolvedProxy},
    };
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
        routing::CapabilityId,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use url::Url;

    use super::bind_execution_identity;
    use crate::{
        adapter::{Authentication, ExecutionContext},
        connection::{ConnectionKey, ConnectionPool, physical::ConnectionProtocol},
        protocol::RequestIdentity,
        routing::CredentialGeneration,
    };

    /// The route whose credential one execution signs with.
    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new("spot").expect("spot is a canonical scope"),
        )
    }

    fn context(account: &str) -> ExecutionContext {
        let transport = Arc::new(TransportHub::new(TransportPolicy::default()));
        let pool = Arc::new(ConnectionPool::with_transport(transport));
        let identity = RequestIdentity::new(route(), CapabilityId::AuthenticatedTrading);
        let origins = OriginPair {
            http: "https://api.binance.com"
                .parse()
                .expect("the origin parses"),
            websocket: "wss://stream.binance.com:9443"
                .parse()
                .expect("the origin parses"),
        };
        ExecutionContext::new(identity, origins, reqwest::Client::new(), pool)
            .with_credential_generation(Some(CredentialGeneration::new(3)))
            .with_authentication(Authentication::ApiKey {
                account_id: account.into(),
                api_key: "api-key".into(),
                secret: "secret".into(),
            })
    }

    fn key() -> ConnectionKey {
        ConnectionKey::for_route(
            Url::parse("wss://example.test/ws").expect("the fixture endpoint should parse"),
            ResolvedProxy::NoProxy,
            None,
            route(),
            NetworkRouteGeneration::default(),
            &ConnectionProtocol::new(|_| None),
        )
    }

    /// The account, the route and the generation of one execution reach the key,
    /// which is what a reauthentication reads them back from.
    #[test]
    fn the_execution_identity_is_bound_onto_the_key() {
        let bound = bind_execution_identity(key(), &context("account"))
            .expect("the fixture account is a valid identity");

        assert_eq!(
            bound.account.as_ref().map(|account| account.as_str()),
            Some("account")
        );
        assert_eq!(bound.route, route());
        assert_eq!(
            bound.credential_generation,
            Some(CredentialGeneration::new(3))
        );
    }

    /// Two accounts of one route are two connections: a socket opened for one is
    /// never reused for the other.
    #[test]
    fn two_accounts_of_one_route_are_different_connections() {
        let first = bind_execution_identity(key(), &context("account"))
            .expect("the fixture account is a valid identity");
        let second = bind_execution_identity(key(), &context("other-account"))
            .expect("the fixture account is a valid identity");

        assert_ne!(first, second);
    }
}
