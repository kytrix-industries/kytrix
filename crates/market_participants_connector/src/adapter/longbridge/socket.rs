//! The venue's quote connection: the endpoint a region resolves to, the two headers
//! the venue routes by, and the authenticated session one dial establishes.
//!
//! A Longbridge socket is one venue-level session rather than one per trading
//! account, so the connection this module opens is keyed by the venue's own account
//! identity and by the credentials that identity was read from. Two things follow
//! from that, and both are stated where they are decided rather than at a use site:
//!
//! * The endpoint is the region's own origin with the protocol's query appended, so a
//!   region change is a different endpoint and therefore a different connection key.
//! * The upgrade request carries `accept-language` and `x-dc-region`, declared on the
//!   protocol so that the key the pool reuses a socket by and the dial that opened it
//!   are one declaration. A socket dialled for one data centre can never answer a
//!   request that needs another.
//!
//! The first handshake of a socket is made by the pool's own handshake closure — the
//! one-time password, the AUTH frame and its acknowledgement, then the market-data
//! profile — and the closure the pool installs from it is what every later reconnection
//! replays. One socket is therefore authenticated exactly once, by one password: the
//! closure's first run spends the password it just fetched, and only a reconnection asks
//! the venue for another.

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

/// How long the quote connection may stay silent before it stops being trusted.
///
/// The official SDK treats a socket it has not heard from — specifically, one that
/// has sent no WebSocket Ping — for 120 seconds as dead, and that is the venue's own
/// bound rather than one this build chose. No keepalive is declared beside it: the
/// venue sends its own pings, and the connection layer counts every inbound frame,
/// pings included, as liveness.
pub(super) const LIVENESS_TIMEOUT: Duration = Duration::from_secs(120);

/// The path of the venue's quote socket (§4.1).
const QUOTE_SOCKET_PATH: &str = "/v2";
/// The query the venue's own client appends to that path: protocol version 1, the
/// protobuf codec, and the OpenAPI platform.
const QUOTE_SOCKET_QUERY: [(&str, &str); 3] = [("version", "1"), ("codec", "1"), ("platform", "9")];

/// The header the venue routes a socket by (§4.1).
const DATA_CENTRE_HEADER: &str = "x-dc-region";
/// The data centre a US credential set is served from.
const US_DATA_CENTRE: &str = "us";
/// The data centre everything else is served from, which is the gateway's default.
const ASIA_DATA_CENTRE: &str = "ap";
/// The prefix that marks one credential value as issued in the US.
const US_CREDENTIAL_PREFIX: &str = "us_";

/// The data centre one credential set belongs to.
///
/// The venue routes a request by the region its credentials were issued in, which is
/// independent of the site the operator chose: any one of the three values starting
/// with `us_` is a US credential, and everything else is served from the gateway's
/// own default. The header is a transport fact — a socket served from one data centre
/// cannot answer for another — so this is what the connection key is built from.
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

/// The two headers every Longbridge request carries.
///
/// The language is the venue's own default rather than the interface's: it decides
/// how the venue words the descriptions this build does not read. The data centre is
/// derived from the credentials, and the same map is declared on the protocol that
/// dials the socket and put on the HTTP requests beside it.
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

/// The quote socket of one execution's region.
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

/// The protocol one connection dials with and is keyed by.
///
/// The declaration and the key are one value: the upgrade headers live on the
/// protocol, and [`ConnectionKey::for_route`] reads them from there, so a socket
/// dialled for one data centre cannot be found under the key of another.
fn protocol(headers: &HeaderMap) -> ConnectionProtocol {
    ConnectionProtocol::new(codec::extract_response_id)
        .with_upgrade_header_map(headers)
        .with_liveness_timeout(LIVENESS_TIMEOUT)
}

/// The key of the venue's one quote connection.
fn connection_key(
    context: &ExecutionContext,
    endpoint: Url,
    protocol: &ConnectionProtocol,
) -> ConnectionKey {
    let mut key = ConnectionKey::for_route(
        endpoint,
        context.proxy.clone(),
        // The venue's socket is authenticated by the one-time password it is handed,
        // not by an execution's credential, so there is no authentication identity
        // beside the account.
        None,
        context.route(),
        context.network_route_generation(),
        protocol,
    );
    // The account is the venue itself. The connection layer reads it back when it
    // authenticates a socket, so a key without one could never complete a
    // reauthentication; the credential generation travels with it, because two
    // generations share an endpoint and are still two different sessions.
    key.with_credentials(
        Some(
            MarketParticipantAccountId::new(auth::ACCOUNT)
                .expect("the venue-level account identity is canonical"),
        ),
        context.credential_generation(),
    );
    key
}

/// One connection as it is declared: what it is keyed by, what dials it and what it
/// presents.
///
/// The three values are one declaration rather than three that have to be kept in
/// step: the upgrade headers live on the protocol, the key reads them from that same
/// protocol, and the dial materialises the same protocol's declaration. A caller that
/// holds this value can therefore ask whether a cached session belongs to the
/// connection a request would dial without building a second, possibly different,
/// declaration.
pub(super) struct ConnectionPlan {
    /// The key the pool holds this connection under.
    pub key: ConnectionKey,
    /// The protocol whose declaration every dial of it presents.
    protocol: ConnectionProtocol,
}

impl ConnectionPlan {
    /// Declares the venue's one quote connection for one credential set.
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

/// Opens or reuses the venue's quote connection.
///
/// The dial presents what the venue's own client presents: the one-time password as an
/// AUTH frame, which the connection layer replays as the first thing every socket does —
/// the first one and every reconnection alike. The password is asked of the venue only on
/// the run that dials: a socket the pool already holds and that this build already
/// authenticated is one this build may send on, and a password fetched for a socket
/// nothing dialled is a request the venue counts for nothing.
///
/// The password travels back to the caller as well, because the state the layer's own
/// actor establishes from it is what [`establish`] records — the same two values, written
/// once.
///
/// The state is `stateful` because the socket carries credentials and is kept for the
/// requests that use them, which is exactly what the state means.
///
/// A pooled hit installs nothing: the closure the connection already carries is the one
/// its own socket was authenticated by, and re-running it would be a second handshake on
/// a socket that has had one.
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
    // The closure below is the connection's whole authentication: the pool installs what
    // it answers as the reauthentication of the socket it is about to dial, and the
    // connection layer runs that closure before the socket is handed back. Its first run
    // therefore is the handshake, and it answers with the password it has just fetched;
    // every later run is a reconnection and asks the venue for a new one.
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
                // The frame this connection is opened with is also the frame every
                // reconnection replays, because both are built by the same function: the
                // closure's first run spends the password it has just fetched, and a
                // later run asks the venue for a new one.
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

/// Establishes the authenticated session of one planned connection.
///
/// The steps are the venue's own order and each one fails the whole acquisition. The
/// one-time password is fetched *before* the socket is dialled — by [`dial`], inside the
/// pool's own miss — because the venue refuses a socket to an account that is out of
/// connection slots and a refusal must cost no socket: fetching first means a refused
/// account never opens one, and the socket the pool would have counted towards the
/// venue's own `online` is never dialled.
///
/// The AUTH frame itself is the connection layer's, replayed on the new socket before it
/// is handed back, and it is built by the very closure [`dial`] handed the pool: that
/// closure answers with the password it just fetched, so the password that authenticates
/// the socket is the one fetched before the dial rather than a second one asked for
/// afterwards. This call therefore records the state that handshake established and
/// installs no second closure: a socket the pool already holds and that this build
/// already authenticated needs no password at all, and one that was just dialled is
/// authenticated by the time this call sees it.
pub(super) async fn establish(
    context: &ExecutionContext,
    credentials: &LongbridgeCredentialValues,
    plan: &ConnectionPlan,
) -> Result<Arc<LongbridgeSession>, MarketParticipantError> {
    let (connection, otp) = dial(context, credentials, plan).await?;
    if let Some(otp) = otp {
        // The same call and the same two values the connection layer writes for every
        // reconnection ([`Connection::authenticate`]): the venue-level account and the
        // password that opened the socket.
        connection.authenticate(auth::ACCOUNT, otp);
    }

    let mut session = LongbridgeSession::pending(Arc::clone(&connection), plan.key.clone());
    let profile = auth::fetch_profile(&session, &context.route()).await?;
    session.install_profile(&profile);
    Ok(Arc::new(session))
}
