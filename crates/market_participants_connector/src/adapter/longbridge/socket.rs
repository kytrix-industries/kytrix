use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use market_participants_model::identifier::MarketParticipantAccountId;
use reqwest::header::{ACCEPT_LANGUAGE, HeaderMap, HeaderName, HeaderValue};
use smol_str::SmolStr;
use url::Url;

use super::{
    auth::{self, LongbridgeSession},
    codec,
};
use crate::{
    adapter::ExecutionContext,
    connection::{
        Connection, ConnectionKey, ConnectionState, ReconnectPolicy, physical::ConnectionProtocol,
    },
    error::MarketParticipantError,
    runtime::LongbridgeCredentialValues,
};

pub(super) const LIVENESS_TIMEOUT: Duration = Duration::from_secs(120);

const QUOTE_SOCKET_PATH: &str = "/v2";
const QUOTE_SOCKET_QUERY: [(&str, &str); 3] = [("version", "1"), ("codec", "1"), ("platform", "9")];

const DATA_CENTRE_HEADER: &str = "x-dc-region";
const US_DATA_CENTRE: &str = "us";
const ASIA_DATA_CENTRE: &str = "ap";
const US_CREDENTIAL_PREFIX: &str = "us_";

pub(super) fn data_centre(credentials: &LongbridgeCredentialValues) -> &'static str {
    let us = [
        &credentials.app_key,
        &credentials.app_secret,
        &credentials.access_token,
    ]
    .iter()
    .any(|value| value.starts_with(US_CREDENTIAL_PREFIX));
    if us { US_DATA_CENTRE } else { ASIA_DATA_CENTRE }
}

pub(super) fn region_headers(credentials: &LongbridgeCredentialValues) -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(2);
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(auth::PROFILE_LANGUAGE),
    );
    headers.insert(
        HeaderName::from_static(DATA_CENTRE_HEADER),
        HeaderValue::from_static(data_centre(credentials)),
    );
    headers
}

fn websocket_endpoint(context: &ExecutionContext) -> Result<Url, MarketParticipantError> {
    let mut endpoint = context
        .websocket_origin()
        .join(QUOTE_SOCKET_PATH)
        .map_err(|_| {
            MarketParticipantError::Configuration("invalid Longbridge WebSocket path".into())
        })?;
    endpoint.query_pairs_mut().extend_pairs(QUOTE_SOCKET_QUERY);
    Ok(endpoint)
}

fn protocol(headers: &HeaderMap) -> ConnectionProtocol {
    ConnectionProtocol::new(codec::extract_response_id)
        .with_upgrade_header_map(headers)
        .with_liveness_timeout(LIVENESS_TIMEOUT)
}

fn connection_key(
    context: &ExecutionContext,
    endpoint: Url,
    protocol: &ConnectionProtocol,
) -> ConnectionKey {
    let mut key = ConnectionKey::for_route(
        endpoint,
        context.proxy.clone(),
        None,
        context.route(),
        context.network_route_generation(),
        protocol,
    );
    key.with_credentials(
        Some(
            MarketParticipantAccountId::new(auth::ACCOUNT)
                .expect("the venue-level account identity is canonical"),
        ),
        context.credential_generation(),
    );
    key
}

pub(super) struct ConnectionPlan {
    pub key: ConnectionKey,
    protocol: ConnectionProtocol,
}

impl ConnectionPlan {
    pub(super) fn of(
        context: &ExecutionContext,
        credentials: &LongbridgeCredentialValues,
    ) -> Result<Self, MarketParticipantError> {
        let headers = region_headers(credentials);
        let endpoint = websocket_endpoint(context)?;
        let protocol = protocol(&headers);
        let key = connection_key(context, endpoint, &protocol);
        Ok(Self { key, protocol })
    }
}

async fn dial(
    context: &ExecutionContext,
    credentials: &LongbridgeCredentialValues,
    plan: &ConnectionPlan,
) -> Result<(Arc<Connection>, Option<SmolStr>), MarketParticipantError> {
    let pool = Arc::clone(&context.connection_pool);
    let client = context.http_client.clone();
    let origin = context.http_origin().clone();
    let route = context.route();
    let dial_credentials = credentials.clone();
    let route_for_dial = route.clone();
    let first_password: Arc<Mutex<Option<SmolStr>>> = Arc::new(Mutex::new(None));
    let password_for_dial = Arc::clone(&first_password);
    pool.connect_or_create_by_handshake(
        plan.key.clone(),
        ConnectionState::stateful(),
        plan.protocol.clone(),
        pool.cancellation(),
        ReconnectPolicy::default(),
        move || {
            let client = client.clone();
            let origin = origin.clone();
            let route = route_for_dial.clone();
            let credentials = dial_credentials.clone();
            let password = Arc::clone(&password_for_dial);
            Box::pin(async move {
                let otp = auth::fetch_otp(&client, &origin, &route, &credentials).await?;
                *password
                    .lock()
                    .expect("the password record is not poisoned") = Some(otp.clone());
                let reauthentication: crate::connection::Reauthentication = Arc::new(move || {
                    let client = client.clone();
                    let origin = origin.clone();
                    let route = route.clone();
                    let credentials = credentials.clone();
                    let password = Arc::clone(&password);
                    Box::pin(async move {
                        let recorded = password
                            .lock()
                            .expect("the password record is not poisoned")
                            .take();
                        let otp = match recorded {
                            Some(otp) => otp,
                            None => auth::fetch_otp(&client, &origin, &route, &credentials).await?,
                        };
                        auth::authentication_request(&otp, &route)
                    })
                });
                let acknowledgement = auth::authentication_request(&otp, &route_for_dial)?;
                Ok((reauthentication, Some(acknowledgement.session_token)))
            })
        },
    )
    .await
}

pub(super) async fn establish(
    context: &ExecutionContext,
    credentials: &LongbridgeCredentialValues,
    plan: &ConnectionPlan,
) -> Result<Arc<LongbridgeSession>, MarketParticipantError> {
    let (connection, otp) = dial(context, credentials, plan).await?;
    if let Some(otp) = otp {
        connection.authenticate(auth::ACCOUNT, otp);
    }

    let mut session = LongbridgeSession::pending(Arc::clone(&connection), plan.key.clone());
    let profile = auth::fetch_profile(&session, &context.route()).await?;
    session.install_profile(&profile);
    Ok(Arc::new(session))
}
