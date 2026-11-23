use market_participants_model::identifier::MarketParticipantAccountId;

use crate::{
    adapter::ExecutionContext,
    connection::ConnectionKey,
    error::{AuthError, MarketParticipantError},
};

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

    #[test]
    fn two_accounts_of_one_route_are_different_connections() {
        let first = bind_execution_identity(key(), &context("account"))
            .expect("the fixture account is a valid identity");
        let second = bind_execution_identity(key(), &context("other-account"))
            .expect("the fixture account is a valid identity");

        assert_ne!(first, second);
    }
}
