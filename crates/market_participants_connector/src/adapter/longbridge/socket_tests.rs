//! The tests of the connection module: the plan one dial is declared by, the handshake
//! it reaches the wire with, and the refusals each step of it is answered with.

use std::sync::Arc;

use market_participants_model::routing::CapabilityId;
use prost::Message as _;
use sha1::Digest as _;
use smol_str::{SmolStr, format_smolstr};

use super::socket::{ConnectionPlan, data_centre, establish, region_headers};
use crate::{
    adapter::longbridge::{
        codec,
        signature::hex_hmac_sha256,
        test_support::{
            AuthAnswer, LongbridgeFixture, ProfileAnswer, TokenAnswer, credential_values,
            credentials, execution,
        },
    },
    connection::ConnectionState,
    error::{AuthError, MarketParticipantError},
    runtime::LongbridgeCredentialValues,
};

/// One header of one request the fixture answered, as the request spelled it.
fn read(headers: &[(String, String)], name: &str) -> SmolStr {
    headers
        .iter()
        .find(|(header, _)| header == name)
        .map(|(_, value)| SmolStr::new(value))
        .unwrap_or_else(|| panic!("the request carries no {name} header: {headers:?}"))
}

/// One credential set whose first value is `value`.
fn starting_with(value: &'static str) -> LongbridgeCredentialValues {
    LongbridgeCredentialValues {
        app_key: SmolStr::new(value),
        app_secret: SmolStr::new_static("app-secret"),
        access_token: SmolStr::new_static("access-token"),
    }
}

/// One plan of one fixture execution.
fn plan(context: &crate::adapter::ExecutionContext) -> ConnectionPlan {
    ConnectionPlan::of(context, &credential_values()).expect("the fixture connection is planned")
}

/// The data centre is the credentials' own: any one of the three values that
/// starts with `us_` is a US credential, and everything else is the gateway's
/// Asian default.
#[test]
fn the_data_centre_is_derived_from_the_credentials() {
    assert_eq!(data_centre(&credential_values()), "ap");
    assert_eq!(
        data_centre(&starting_with("us_app-key")),
        "us",
        "the app key alone is enough"
    );
    assert_eq!(data_centre(&starting_with("us_anything")), "us");
    assert_eq!(
        data_centre(&starting_with("app_key")),
        "ap",
        "a value that merely contains the prefix is not a US credential"
    );
    assert_eq!(
        data_centre(&starting_with("US_app-key")),
        "ap",
        "the prefix is case-sensitive"
    );
}

/// Both headers every Longbridge request carries are the design's own: the
/// language the venue words its answers in, and the region of the credentials.
#[test]
fn the_two_region_headers_are_the_designs_own() {
    let headers = region_headers(&starting_with("us_app-key"));
    let language = headers
        .get("accept-language")
        .and_then(|value| value.to_str().ok());
    let centre = headers
        .get("x-dc-region")
        .and_then(|value| value.to_str().ok());

    assert_eq!(language, Some("en"));
    assert_eq!(centre, Some("us"));
    assert_eq!(headers.len(), 2, "and nothing else is declared");
}

/// The endpoint is the region's own origin with the protocol's query appended, the
/// account is the venue itself, and the key counts exactly the headers the dial
/// presents — so a region change is a different key rather than a reused socket.
#[tokio::test]
async fn the_planned_connection_is_the_regions_own() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);
    let plan = plan(&context);

    assert_eq!(plan.key.endpoint.path(), "/v2");
    assert_eq!(
        plan.key.endpoint.query(),
        Some("version=1&codec=1&platform=9")
    );
    assert_eq!(
        plan.key.endpoint.origin(),
        context.websocket_origin().origin(),
        "the endpoint is the region's own origin"
    );
    assert_eq!(
        plan.key.account.as_ref().map(|account| account.as_str()),
        Some("longbridge"),
        "the socket authenticates as the venue rather than as a trading account"
    );
    assert_eq!(
        plan.key.credential_generation,
        context.credential_generation(),
        "and the credentials it was dialled for travel with it"
    );
    assert_eq!(
        plan.key.upgrade_headers.to_header_map(),
        region_headers(&credential_values()),
        "the key counts exactly the headers the dial presents"
    );
}

/// The handshake reaches the wire in the venue's own order: the OTP over HTTP with
/// both headers, then AUTH with request id zero, then the market-data profile —
/// and the socket's own upgrade carried the two headers the venue routes by.
#[tokio::test]
async fn the_handshake_reaches_the_wire_in_the_venues_own_order() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);

    let session = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect("the fixture venue authenticates the connection");

    let frames = fixture.settled_frames().await;
    assert_eq!(
        frames.iter().map(|frame| frame.command).collect::<Vec<_>>(),
        vec![codec::COMMAND_CODE_AUTH, codec::QUERY_USER_QUOTE_PROFILE],
        "the one-time password, then AUTH, then the profile"
    );
    assert_eq!(
        frames[0].request_id, 0,
        "the AUTH frame carries id zero, which no pending request of the connection layer can be"
    );
    let auth = longbridge_proto::control::AuthRequest::decode(frames[0].body.as_ref())
        .expect("the AUTH body is the venue's own message");
    assert_eq!(
        auth.token, "fixture-otp",
        "the frame carries the password the token endpoint handed out"
    );
    assert_eq!(
        auth.metadata.get("accept-language").map(String::as_str),
        Some("en")
    );
    assert_eq!(
        auth.metadata
            .get("need_over_night_quote")
            .map(String::as_str),
        Some("true"),
        "the overnight session is asked for"
    );
    assert!(
        frames[1].request_id >= 1,
        "the profile is an ordinary request with an id the connection layer allocated: {}",
        frames[1].request_id
    );

    let token_headers = fixture.token_header_blocks();
    assert_eq!(
        token_headers.len(),
        1,
        "one socket was opened, and it asked for one password: the password the dial \
         fetched is the one its own socket authenticated with"
    );
    assert!(
        token_headers[0]
            .iter()
            .any(|(name, value)| name == "accept-language" && value == "en"),
        "the OTP request states its language: {:?}",
        token_headers[0]
    );
    assert!(
        token_headers[0]
            .iter()
            .any(|(name, value)| name == "x-dc-region" && value == "ap"),
        "the OTP request states its data centre: {:?}",
        token_headers[0]
    );

    // The request that reaches the venue is signed, and the signature is the one
    // §4.9 states for the values it presented: the fixture recomputes it the way
    // the venue does — over the token, the key and the instant the headers carry —
    // and the two have to agree. An unsigned request, or one that signed a value
    // it then changed, is what the live run measured as `403201 signature invalid`.
    let signings = fixture.token_signings();
    assert_eq!(signings.len(), 1, "one signed token request");
    let (presented, computed) = &signings[0];
    assert!(
        presented.starts_with(
            "HMAC-SHA256 SignedHeaders=authorization;x-api-key;x-timestamp, Signature="
        ),
        "the signature travels in the venue's own header line: {presented}"
    );
    assert_eq!(
        presented, computed,
        "the OTP request's signature is the one its own values produce (§4.9)"
    );
    assert_eq!(
        presented
            .rsplit_once("Signature=")
            .expect("the line states its signature")
            .1
            .len(),
        64,
        "and what it carries is one SHA-256 in hexadecimal"
    );

    let upgrades = fixture.upgrade_header_blocks();
    assert_eq!(upgrades.len(), 1, "one socket was opened");
    assert!(
        upgrades[0]
            .iter()
            .any(|(name, value)| name == "x-dc-region" && value == "ap"),
        "the upgrade itself carries the header the venue routes by: {:?}",
        upgrades[0]
    );
    assert!(
        upgrades[0]
            .iter()
            .any(|(name, value)| name == "accept-language" && value == "en"),
        "and the language: {:?}",
        upgrades[0]
    );

    assert!(session.us_market_data, "the profile granted US market data");
    // The first handshake writes the same authenticated state the connection layer
    // writes for every reconnection: the venue-level account and the password the
    // socket was opened with. A socket whose state said otherwise would be
    // handshaken a second time by the next request that found it in the pool.
    let ConnectionState::Stateful {
        authenticated,
        account_id,
        session_token,
    } = session.connection().state()
    else {
        panic!("the venue's socket is a stateful one");
    };
    assert!(
        authenticated,
        "the socket this call authenticated is authenticated"
    );
    assert_eq!(
        account_id.as_ref().map(SmolStr::as_str),
        Some("longbridge"),
        "the state names the venue the socket authenticates as"
    );
    assert_eq!(
        session_token.as_ref().map(SmolStr::as_str),
        Some("fixture-otp"),
        "and the one-time password that opened it"
    );
}

/// A socket the pool already holds and that this build already authenticated is
/// not handshaken again: the state the first call wrote is what the second call
/// reads, so the venue sees one AUTH frame and hands out one password per socket
/// rather than one of each per request.
#[tokio::test]
async fn a_healthy_pooled_socket_is_not_handshaken_twice() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);

    let first = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect("the fixture venue authenticates the connection");
    let second = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect("the pooled socket answers the same connection again");

    assert!(
        Arc::ptr_eq(first.connection(), second.connection()),
        "both calls answer the connection the pool holds"
    );
    assert_eq!(
        fixture.commands(),
        vec![
            codec::COMMAND_CODE_AUTH,
            codec::QUERY_USER_QUOTE_PROFILE,
            codec::QUERY_USER_QUOTE_PROFILE
        ],
        "one AUTH frame for the socket, and a profile read per session"
    );
    assert_eq!(
        fixture.upgrade_header_blocks().len(),
        1,
        "and one socket was opened: the second call dialled nothing"
    );
    assert_eq!(
        fixture.token_header_blocks().len(),
        1,
        "so one password was asked for, by the dial that opened it"
    );
}

/// A venue that is out of connection slots is refused with its own numbers, and
/// the refusal is worth retrying: another session may already have ended.
///
/// The password is what answers the refusal, and it is fetched before anything is
/// dialled, so the socket this build would have counted towards the venue's own
/// `online` is never opened: a refusal that cost a socket would be a refusal that
/// made the account's own situation worse.
#[tokio::test]
async fn a_venue_with_no_free_connection_slot_states_its_own_numbers() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer {
            limit: 3,
            online: 5,
            ..TokenAnswer::granted("fixture-otp")
        },
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);

    let error = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect_err("a venue with no free slot hands out no password");

    let MarketParticipantError::Rejected {
        code,
        message,
        retryable,
        raw_response,
    } = &error
    else {
        panic!("a venue out of slots is a refusal: {error}");
    };
    assert_eq!(code, "connection-limit");
    assert!(
        message.contains("5 of its 3"),
        "the venue's own numbers travel in the sentence: {message}"
    );
    assert!(retryable);
    assert!(raw_response.is_none());
    assert!(error.is_retryable());
    assert_eq!(
        fixture.token_header_blocks().len(),
        1,
        "the password was asked for once"
    );
    assert!(
        fixture.frames().is_empty(),
        "and no frame was sent: there was no socket to send one on"
    );
    assert!(
        fixture.upgrade_header_blocks().is_empty(),
        "the refusal was answered before anything was dialled, so the venue's own \
         connection count was never raised by this build"
    );
}

/// A token endpoint the venue refuses is the refusal it is, and the code a reader
/// gets is the venue's own envelope code rather than the transport status: a 5xx
/// that states its own reason is answered with that reason, and the failure is worth
/// retrying because it is the venue failing to answer rather than declining.
#[tokio::test]
async fn a_refused_token_endpoint_is_the_refusal_it_is() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::refused(500_001, "the fixture refuses this request", 500),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);

    let error = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect_err("a refused token request hands out no password");

    let MarketParticipantError::Rejected {
        code,
        message,
        retryable,
        raw_response,
    } = &error
    else {
        panic!("a refused token request is a refusal: {error}");
    };
    assert_eq!(code, "500001", "the venue's own envelope code");
    assert_eq!(message, "the fixture refuses this request");
    assert!(retryable, "a server failure is worth retrying");
    assert!(
        raw_response.is_some(),
        "the venue's own body travels with the refusal"
    );
    assert!(fixture.frames().is_empty(), "no socket was opened");
}

/// A refused authentication fails the whole acquisition, typed as the credential
/// failure it is, and the profile is never asked for.
#[tokio::test]
async fn a_refused_authentication_fails_the_connection() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer { status: 1 },
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);

    let error = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect_err("a refused authentication is not a session");

    assert!(
        matches!(
            error,
            MarketParticipantError::AuthenticationFailed {
                account: Some(ref account),
                source: AuthError::InvalidCredentials,
                ..
            } if account == "longbridge"
        ),
        "{error}"
    );
    assert_eq!(
        fixture.settled_frames().await.len(),
        1,
        "the profile was never asked for after the refusal"
    );
}

/// A refused profile fails the whole acquisition as the venue's own refusal, which
/// is what a market-data profile that cannot be read is: there is no pace to honour
/// and no permission to judge a command by.
#[tokio::test]
async fn a_refused_profile_fails_the_connection() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer {
            status: 1,
            ..ProfileAnswer::granted()
        },
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);

    let error = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect_err("a refused profile is not a session");

    assert!(
        matches!(
            &error,
            MarketParticipantError::Rejected { code, message, .. }
                if code == "4000000" && message == "the fixture refuses the profile"
        ),
        "{error}"
    );
}

/// The OTP request the adapter actually sends is signed, and the signature is the
/// one §4.9's algorithm states for the values it presents.
///
/// The live run is the reason this is an assertion rather than a description: the
/// venue answered `401 {"code":401001,"message":"token empty"}` to the two-header
/// request this build sent before, so a request without the four signing headers is
/// not one the venue serves. The fixture asserts on what arrived rather than on
/// what was configured, and the arm computed over a different message is the
/// control: it fails whenever the signature is a value merely compared with itself.
#[tokio::test]
async fn the_otp_request_is_signed_with_the_algorithm_the_venue_accepts() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, credentials(), CapabilityId::Candlesticks);

    establish(&context, &credential_values(), &plan(&context))
        .await
        .expect("the fixture venue authenticates the connection");

    let headers = fixture.token_header_blocks();
    assert_eq!(headers.len(), 1, "the dial asked for one password");
    let presented: Vec<&str> = headers[0].iter().map(|(name, _)| name.as_str()).collect();
    // The six this build states: the two the venue routes by, the three the
    // signature covers, and the signature itself. The client adds its own transport
    // headers beside them, which is why the list is not asserted to be exactly six.
    for name in [
        "accept-language",
        "x-dc-region",
        "authorization",
        "x-api-key",
        "x-timestamp",
        "x-api-signature",
    ] {
        assert!(
            presented.contains(&name),
            "the OTP request carries {name}: {presented:?}"
        );
    }

    // What the request signed is read off the request itself, and the signature it
    // carried is recomputed from those same values: a request that signed a stale
    // instant, or a value it then changed, is exactly what the venue refuses.
    let credentials = credential_values();
    let signed_values = format!(
        "authorization:{}\nx-api-key:{}\nx-timestamp:{}\n",
        read(&headers[0], "authorization"),
        read(&headers[0], "x-api-key"),
        read(&headers[0], "x-timestamp"),
    );
    let digest = const_hex::encode(sha1::Sha1::digest(
        format!("GET|/v1/socket/token||{signed_values}|authorization;x-api-key;x-timestamp|")
            .as_bytes(),
    ));
    let expected = format_smolstr!(
        "HMAC-SHA256 SignedHeaders=authorization;x-api-key;x-timestamp, Signature={}",
        hex_hmac_sha256(&credentials.app_secret, &format!("HMAC-SHA256|{digest}"))
    );
    assert_eq!(
        read(&headers[0], "x-api-signature"),
        expected,
        "the signature the OTP request presented is §4.9's over its own values"
    );
    assert_eq!(
        expected,
        fixture.token_signings()[0].1,
        "which is the line the fixture's own recomputation states for the same values"
    );
    assert_ne!(
        expected,
        format_smolstr!(
            "HMAC-SHA256 SignedHeaders=authorization;x-api-key;x-timestamp, Signature={}",
            hex_hmac_sha256(&credentials.app_secret, &format!("HMAC-SHA256|{digest}1"))
        ),
        "and the comparison above is not a value compared with itself"
    );
}
