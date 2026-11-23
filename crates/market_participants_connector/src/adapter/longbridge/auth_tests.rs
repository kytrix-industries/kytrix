use std::time::Duration;

use bytes::Bytes;
use market_participants_model::routing::CapabilityId;
use prost::Message as _;

use super::auth::{authentication_request, fetch_profile};
use crate::{
    adapter::longbridge::{
        codec,
        socket::{ConnectionPlan, establish},
        test_support::{
            AuthAnswer, LongbridgeFixture, ProfileAnswer, TokenAnswer, credential_values,
            credentials, execution, route,
        },
    },
    error::{AuthError, MarketParticipantError},
};

fn plan(context: &crate::adapter::ExecutionContext) -> ConnectionPlan {
    ConnectionPlan::of(context, &credential_values()).expect("the fixture connection is planned")
}

fn token(limit: i32, online: i32) -> TokenAnswer {
    TokenAnswer {
        limit,
        online,
        ..TokenAnswer::granted("fixture-otp")
    }
}

#[tokio::test]
async fn a_refusal_inside_the_envelope_is_the_venues_own_code_and_message() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::refused(403_201, "signature invalid", 200),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);

    let error = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect_err("a venue that declines in its envelope hands out no password");

    let MarketParticipantError::Rejected {
        code,
        message,
        retryable,
        raw_response,
    } = &error
    else {
        panic!("a declined token request is a refusal: {error}");
    };
    assert_eq!(code, "403201", "the venue's own code, not the HTTP status");
    assert_eq!(message, "signature invalid");
    assert!(!retryable, "the same signature would be declined again");
    assert!(
        raw_response.is_some(),
        "the envelope travels with the refusal"
    );
    assert!(fixture.frames().is_empty(), "no socket was opened");
}

#[tokio::test]
async fn an_envelope_without_its_payload_is_not_a_token() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp").without_payload(),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);

    let error = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect_err("an envelope with no payload hands out no password");

    assert!(
        matches!(
            error,
            MarketParticipantError::DecodeFailed { ref operation, .. }
                if operation == "decode the Longbridge socket token"
        ),
        "{error}"
    );
    assert!(fixture.frames().is_empty(), "no socket was opened");
}

#[tokio::test]
async fn the_connection_limit_is_the_venues_own_comparison() {
    let fixture = LongbridgeFixture::install(
        token(0, 0),
        AuthAnswer::accepted(),
        ProfileAnswer::granted(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);

    let error = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect_err("a venue that reports no limit is read as one with none left");

    assert!(
        matches!(
            &error,
            MarketParticipantError::Rejected { code, message, retryable, .. }
                if code == "connection-limit"
                    && message == "the venue reports 0 of its 0 connections in use"
                    && *retryable
        ),
        "{error}"
    );
}

fn answer_frame(command: u8, status: u8, body: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(10 + body.len());
    frame.push(2);
    frame.push(command);
    frame.extend_from_slice(&1_u32.to_be_bytes());
    frame.push(status);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    frame.extend_from_slice(body);
    Bytes::from(frame)
}

#[tokio::test]
async fn a_market_data_command_without_permission_is_refused_before_a_frame() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::without_us_market_data(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let session = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect("the fixture venue authenticates the connection");
    assert!(!session.us_market_data, "the fixture granted nothing");
    let before = fixture.frames().len();

    let error = session
        .request::<longbridge_proto::quote::UserQuoteProfileResponse>(
            &context.route(),
            codec::GET_SECURITY_HISTORY_CANDLESTICKS,
            &longbridge_proto::quote::UserQuoteProfileRequest {
                language: "en".to_owned(),
            },
        )
        .await
        .expect_err("a command the profile grants no permission for is refused");

    assert!(
        matches!(
            error,
            MarketParticipantError::AuthenticationFailed {
                account: Some(ref account),
                source: AuthError::NoMarketDataPermission,
                ..
            } if account == "longbridge"
        ),
        "{error}"
    );
    assert!(
        !error.is_retryable(),
        "the same credentials would be refused again"
    );
    assert_eq!(
        fixture.frames().len(),
        before,
        "no frame was sent: the gate is what refused the command"
    );
}

#[tokio::test]
async fn the_profile_is_asked_for_even_when_no_permission_is_granted() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer::without_us_market_data(),
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);

    let session = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect("a profile that grants nothing is still a profile");

    assert!(!session.us_market_data);
    assert_eq!(
        fixture.commands(),
        vec![codec::COMMAND_CODE_AUTH, codec::QUERY_USER_QUOTE_PROFILE]
    );
}

#[tokio::test]
async fn a_paced_command_waits_for_its_token() {
    let fixture = LongbridgeFixture::install(
        TokenAnswer::granted("fixture-otp"),
        AuthAnswer::accepted(),
        ProfileAnswer {
            rate_limit: vec![(codec::QUERY_USER_QUOTE_PROFILE, 1, 1)],
            ..ProfileAnswer::granted()
        },
    )
    .await;
    let context = execution(&fixture, Some(credentials()), CapabilityId::Candlesticks);
    let session = establish(&context, &credential_values(), &plan(&context))
        .await
        .expect("the fixture venue authenticates the connection");
    assert!(
        session
            .quotas
            .contains_key(&codec::QUERY_USER_QUOTE_PROFILE),
        "the profile installed a bucket for its own command"
    );

    let burst = fetch_profile(&session, &context.route())
        .await
        .expect("the burst request is answered");
    assert_eq!(burst.member_id, 1);

    let started = std::time::Instant::now();
    let paced = fetch_profile(&session, &context.route())
        .await
        .expect("the paced request is answered after its token");

    assert_eq!(paced.member_id, 1, "the answer is the venue's own");
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the request waited for the bucket's next token, waited {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "and it waited for the venue's pace rather than for a timeout, waited {:?}",
        started.elapsed()
    );
}

#[test]
fn the_acknowledgement_matches_the_authentications_content() {
    let request =
        authentication_request("fixture-otp", &route()).expect("the fixture authentication builds");

    assert_eq!(request.payload[0], 1, "the payload is a request frame");
    assert_eq!(
        u32::from_be_bytes([
            request.payload[2],
            request.payload[3],
            request.payload[4],
            request.payload[5],
        ]),
        0,
        "the frame carries request id zero"
    );
    let body = longbridge_proto::control::AuthRequest::decode(&request.payload[11..])
        .expect("the frame carries the venue's own authentication message");
    assert_eq!(body.token, "fixture-otp");
    assert_eq!(request.session_token, "fixture-otp");

    let accepted = answer_frame(codec::COMMAND_CODE_AUTH, 0, &[]);
    assert!(
        (request.acknowledged)(&accepted)
            .expect("an accepted authentication is an acknowledgement")
            .is_some()
    );

    let refused = answer_frame(codec::COMMAND_CODE_AUTH, 1, &[]);
    assert!(matches!(
        (request.acknowledged)(&refused),
        Err(MarketParticipantError::AuthenticationFailed {
            source: AuthError::InvalidCredentials,
            ..
        })
    ));

    let other = answer_frame(codec::QUERY_USER_QUOTE_PROFILE, 0, &[]);
    assert!(
        (request.acknowledged)(&other)
            .expect("another command's answer is not this handshake's")
            .is_none()
    );
    let push = {
        let mut frame = vec![3_u8, 101];
        frame.extend_from_slice(&0_u32.to_be_bytes()[1..]);
        Bytes::from(frame)
    };
    assert!(
        (request.acknowledged)(&push)
            .expect("a push is not an authentication answer")
            .is_none()
    );
}
