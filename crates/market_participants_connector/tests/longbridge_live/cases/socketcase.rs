//! The socket-lifecycle cases: a reconnect replays the handshake, and the venue's own
//! liveness signalling (§4.4, §10.4; §14.1 item 11).

use std::time::Duration;

use longbridge_proto::quote::{SubType, SubscribeRequest};
use prost::Message as _;
use tokio::time::timeout;

use crate::{
    bounds::{CASE_TIMEOUT, PING_OBSERVATION, PUSH_WINDOW, RECONNECT_TIMEOUT},
    client::chosen_transport,
    credentials::Credentials,
    skip_without_credentials,
    socket::LiveSocket,
    wire::{COMMAND_CODE_AUTH, SUB_TYPE_QUOTE, SUB_TYPE_TRADE, SUBSCRIBE},
};
/// Case 15 — reconnecting replays the whole handshake and the subscription
/// (§4.4, §10.4).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_15_a_reconnect_replays_the_handshake_and_the_subscription() {
    let case = "case 15 (reconnect)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let symbol = crate::symbols::symbol();
        let mut socket =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
        let first = socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        let first_session = longbridge_proto::control::AuthResponse::decode(first.body.as_ref())
            .expect("the AUTH answer decodes");
        let subscribe = SubscribeRequest {
            symbol: vec![symbol.clone()],
            sub_type: vec![SubType::Quote as i32, SubType::Trade as i32],
            is_first_push: true,
        };
        socket
            .request_raw(SUBSCRIBE, &subscribe.encode_to_vec())
            .await
            .unwrap_or_else(|error| panic!("{case}: the subscription failed: {error}"));
        let before = socket
            .drain_pushes(tokio::time::Instant::now() + Duration::from_secs(3))
            .await;
        println!(
            "{case}: subscribed to {} {:?}; {} push(es) before the drop; session_id_len={} \
             expires={}",
            symbol,
            [SUB_TYPE_QUOTE, SUB_TYPE_TRADE],
            before.len(),
            first_session.session_id.len(),
            first_session.expires
        );

        // The drop, without a close handshake: this is the connection loss the
        // connection layer has to notice and replay from.
        drop(socket);
        println!("  the socket was dropped without a close handshake");

        let mut replayed =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: the reconnect failed to dial: {error}"));
        let second = timeout(RECONNECT_TIMEOUT, replayed.authenticate())
            .await
            .expect("the reconnect's handshake must finish inside its own bound")
            .unwrap_or_else(|error| panic!("{case}: the reconnect's handshake failed: {error}"));
        let second_session = longbridge_proto::control::AuthResponse::decode(second.body.as_ref())
            .expect("the AUTH answer decodes");
        println!(
            "  reconnected and re-authenticated: session_id_len={} expires={} (a fresh OTP was \
             fetched for this socket)",
            second_session.session_id.len(),
            second_session.expires
        );
        let replay = replayed
            .request_raw(SUBSCRIBE, &subscribe.encode_to_vec())
            .await;
        match &replay {
            Ok(answer) => println!(
                "  the subscription was replayed on the new socket: cmd {} status {:?}",
                answer.command_code, answer.status
            ),
            Err(error) => println!("  the subscription replay failed: {error}"),
        }
        // The replayed subscription is observed for the same window a fresh one gets:
        // a closed market's only push is the snapshot `is_first_push` asks for, and
        // that is exactly what the replay has to produce.
        let after = timeout(PUSH_WINDOW + Duration::from_secs(10), async {
            replayed
                .drain_pushes(tokio::time::Instant::now() + PUSH_WINDOW)
                .await
        })
        .await
        .expect("the replayed subscription's push window must finish");
        println!(
            "  after the replay: {} push(es), command codes {:?}",
            after.len(),
            after
                .iter()
                .map(|push| push.command_code)
                .collect::<Vec<_>>()
        );
        replayed.shutdown().await;

        assert_eq!(second.command_code, COMMAND_CODE_AUTH);
        assert_eq!(second.status, Some(0), "{case}: the second AUTH succeeded");
        assert!(
            replay.is_ok(),
            "{case}: the subscription must be replayable on a reconnected socket"
        );
        assert!(
            !after.is_empty(),
            "{case}: a replayed subscription with is_first_push must answer with a push"
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 16 — the venue's own liveness signalling (§14.1 item 11).
///
/// This case is a measurement and not an assertion: a WebSocket client answers a
/// ping inside its own stack and never hands it to the caller, so the only thing
/// this suite can state is what it observed on the socket it held. The window is
/// shorter than the design's own 120-second liveness bound on purpose.
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_16_the_liveness_window_is_observed_but_pings_are_not_visible() {
    let case = "case 16 (ping cadence)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let mut socket =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
        socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        let started = tokio::time::Instant::now();
        let pushes = socket.drain_pushes(started + PING_OBSERVATION).await;
        let observed = started.elapsed();
        socket.shutdown().await;
        println!(
            "{case}: held an authenticated socket for {:?} without sending anything; {} inbound \
             data frame(s) arrived; the socket stayed healthy for the whole window",
            observed,
            pushes.len()
        );
        println!(
            "  MEASUREMENT NOT TAKEN (partially): the WebSocket client answers the venue's ping \
             inside its own stack and never surfaces it, so a ping cadence cannot be read from \
             this API. The design's LIVENESS_TIMEOUT of 120 s is the venue's own bound and is \
             left at that value; no liveness timeout was observed in {:?}",
            observed
        );
        assert!(
            observed >= PING_OBSERVATION - Duration::from_secs(1),
            "{case}: the observation window must have run its full length"
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}
