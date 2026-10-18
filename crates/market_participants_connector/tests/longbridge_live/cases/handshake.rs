//! The handshake cases: the token endpoint, the region header, the upgrade, AUTH, and the
//! market-data profile the account is read as (§4.1, §4.4, §4.9; §14.1 items 1, 6, 7, 10).
//!
//! Every case dials its own socket and prints what it measured. The signature the token
//! request carries is this suite's own implementation of §4.9's algorithm, which is the
//! point: the adapter's signed `fetch_otp` is checked against the venue by case 17, and
//! this is the control that states the algorithm the venue accepts on its own.

use longbridge_proto::quote::UserQuoteProfileRequest;
use prost::Message as _;
use reqwest::Client;
use tokio::time::timeout;

use crate::{
    bounds::{CASE_TIMEOUT, HTTP_TIMEOUT},
    client::{chosen_transport, live_http_client},
    credentials::Credentials,
    http::{measure_token, token_endpoint},
    skip_without_credentials,
    socket::LiveSocket,
    wire::{
        COMMAND_CODE_AUTH, HEADER_GZIP, HEADER_TYPE_MASK, PACKAGE_TYPE_RESPONSE,
        QUERY_USER_QUOTE_PROFILE, RESPONSE_FIELDS, measure_round_trip,
    },
};

/// Reads one header's value as text.
fn header(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Case 1 — the one-time password the socket is opened with (§4.9).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_01_the_token_endpoint_answers_with_all_three_fields() {
    let case = "case 1 (OTP)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let transport = chosen_transport(&credentials).await;
        let client = live_http_client(transport).expect("the live HTTP client builds");
        let measured = measure_token(&client, &credentials, true)
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        println!(
            "{case}: transport={transport:?} x-dc-region={}",
            credentials.data_centre()
        );
        println!(
            "  GET /v1/socket/token -> status={} envelope_fields={} payload_fields={} limit={} \
             online={} otp_len={} otp_fingerprint={:#018x}",
            measured.status,
            measured.envelope_fields,
            measured.payload_fields,
            measured.limit,
            measured.online,
            measured.otp_length,
            measured.otp_fingerprint
        );
        println!(
            "  §14.1 note: the design writes this 200 as the three fields alone; the venue \
             answered an envelope of {} field(s) whose `data` holds {} — the three the design \
             names are inside it",
            measured.envelope_fields, measured.payload_fields
        );

        assert_eq!(
            measured.status, 200,
            "{case}: the design states a 200 (§4.9)"
        );
        assert_eq!(
            measured.payload_fields, 3,
            "{case}: the design states three fields: otp, limit, online (§4.9)"
        );
        assert!(
            measured.envelope_fields >= 3,
            "{case}: the answer states code, message and data"
        );
        assert!(
            measured.otp_length > 0,
            "{case}: the design states an otp (§4.9)"
        );
        assert!(
            measured.limit > 0,
            "{case}: an account holds at least one connection slot (§4.9)"
        );
        assert!(
            measured.online < measured.limit,
            "{case}: this suite reached the venue, so online ({}) must be below limit ({})",
            measured.online,
            measured.limit
        );

        // The presentation §4.9 described before the first live run: two headers and
        // no signature. This measures what the venue does with it — and the answer is
        // the finding that run reported, not a failure of this case: the signed
        // request above is the one whose shape the case asserts, and the adapter's own
        // OTP request has been signed since that finding (`signature.rs`).
        let unsigned = unsigned_token(&client, &credentials).await;
        println!(
            "  without a signature (what §4.9 described before the first live run, and what this \
             control still sends): {unsigned}"
        );
        assert!(
            unsigned.starts_with("401") || unsigned.starts_with("403"),
            "{case}: the design's unsigned presentation was ACCEPTED; §4.9 is right and the \
             signature this suite adds for the live run is unnecessary. Measured: {unsigned}"
        );
        println!(
            "  §14.1 item 10 measured: the venue refuses an unsigned OTP request, so §4.9's \
             \"OTP 不带 API Key 签名\" was wrong; the adapter's fetch_otp now signs every token \
             request with this same algorithm, pinned by the loopback fixture"
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Asks the token endpoint with the two headers the design describes and no
/// signature, and renders the status together with the venue's own words.
async fn unsigned_token(client: &Client, credentials: &Credentials) -> String {
    let Ok(endpoint) = token_endpoint() else {
        return "<the endpoint is malformed>".to_owned();
    };
    let response = match timeout(
        HTTP_TIMEOUT,
        client
            .get(endpoint)
            .headers(credentials.region_headers())
            .send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return format!("the request failed: {error}"),
        Err(_) => return "the request timed out".to_owned(),
    };
    let status = response.status();
    let body = timeout(HTTP_TIMEOUT, response.bytes())
        .await
        .map(|body| body.unwrap_or_default())
        .unwrap_or_default();
    format!("{status} {}", String::from_utf8_lossy(&body).trim())
}

/// Case 2 — the data centre header, derived from the credentials, and a control
/// pass without it (§4.1; §14.1 item 6).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_02_the_data_centre_is_derived_and_the_header_can_be_omitted() {
    let case = "case 2 (x-dc-region)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let centre = credentials.data_centre();
        println!(
            "{case}: the three credential values are {} long / {} long / {} long; \
             any value starting with `us_`: {} => x-dc-region: {centre}",
            credentials.app_key.len(),
            credentials.app_secret.len(),
            credentials.access_token.len(),
            centre == "us"
        );
        assert_eq!(
            centre, "ap",
            "{case}: the paper-trading HK credential set is an AP credential (§12.9)"
        );
        let transport = chosen_transport(&credentials).await;
        println!("{case}: transport={transport:?}");

        // The first pass presents the header the design says every Longbridge
        // request carries.
        let mut socket = LiveSocket::connect(&credentials, transport, true)
            .await
            .unwrap_or_else(|error| {
                panic!("{case}: the header-carrying connection failed: {error}")
            });
        let ack = socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        println!(
            "  with x-dc-region: {centre}: AUTH ack command_code={} status={:?} frame_len={} \
             declared_body_len={}",
            ack.command_code,
            ack.status,
            ack.frame.len(),
            ack.declared_body_len
        );
        socket.shutdown().await;

        // The control pass presents no data-centre header at all, which is what the
        // venue's gateway default reads as `ap`.
        let client = live_http_client(transport).expect("the live HTTP client builds");
        let control = measure_token(&client, &credentials, false)
            .await
            .unwrap_or_else(|error| panic!("{case}: the control token request failed: {error}"));
        let mut control_socket = LiveSocket::connect(&credentials, transport, false)
            .await
            .unwrap_or_else(|error| panic!("{case}: the control connection failed: {error}"));
        let control_ack = control_socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: the control handshake failed: {error}"));
        control_socket.shutdown().await;

        println!(
            "  without x-dc-region: token status={} limit={} online={} otp_fingerprint={:#018x}; \
             AUTH ack command_code={} status={:?}",
            control.status,
            control.limit,
            control.online,
            control.otp_fingerprint,
            control_ack.command_code,
            control_ack.status
        );
        println!(
            "  difference: both presentations were accepted with identical command answers; \
             with these AP credentials the header cannot be distinguished by this account \
             (see §14.1 item 6)"
        );

        assert_eq!(
            control.status, 200,
            "{case}: the gateway's own default region must still answer a token request"
        );
        assert_eq!(
            control_ack.command_code, COMMAND_CODE_AUTH,
            "{case}: the control connection's AUTH must still be acknowledged"
        );
        assert_eq!(
            control_ack.status,
            Some(0),
            "{case}: the control connection must still authenticate"
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 3 — the upgrade: the endpoint's shape and the two headers it carries
/// (§4.1).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_03_the_upgrade_endpoint_and_headers_are_the_designs_own() {
    let case = "case 3 (WS upgrade)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let endpoint = crate::client::quote_endpoint().expect("the published endpoint parses");
        println!("{case}: endpoint={endpoint}");
        println!(
            "  upgrade headers: accept-language={:?} x-dc-region={:?}",
            header(&credentials.region_headers(), "accept-language"),
            header(&credentials.region_headers(), "x-dc-region")
        );

        assert_eq!(endpoint.scheme(), "wss", "{case}: the quote socket is TLS");
        assert_eq!(
            endpoint.host_str(),
            Some("openapi-quote.longbridge.com"),
            "{case}: the international site's stream host (§4.1)"
        );
        assert_eq!(endpoint.path(), "/v2", "{case}: the venue publishes /v2");
        assert_eq!(
            endpoint.query(),
            Some("version=1&codec=1&platform=9"),
            "{case}: the query the venue's own client appends (§4.1)"
        );

        let headers = credentials.region_headers();
        assert_eq!(
            header(&headers, "accept-language").as_deref(),
            Some("en"),
            "{case}: the design's default language"
        );
        assert_eq!(
            header(&headers, "x-dc-region").as_deref(),
            Some(credentials.data_centre()),
            "{case}: the data centre is the credentials' own"
        );

        // The venue's acceptance of the upgrade is what proves the headers travel:
        // the dial presents exactly these, and a refused handshake would fail here.
        let mut socket =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: the upgrade was refused: {error}"));
        assert_eq!(
            socket.endpoint.query(),
            Some("version=1&codec=1&platform=9")
        );
        println!("  the upgrade completed: the venue accepted the two headers");
        let ack = socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        println!(
            "  AUTH ack command_code={} status={:?} frame_len={}",
            ack.command_code,
            ack.status,
            ack.frame.len()
        );
        socket.shutdown().await;
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 4 — the frame codec: a round-trip, the two length identities, and how many
/// inbound frames carried the gzip and verify bits (§4.2; §14.1 items 9 and 10).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_04_the_codec_round_trips_and_the_venue_frames_measure_clean() {
    let case = "case 4 (frame codec)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        println!("{case}: encoded-then-decoded frames");
        for line in measure_round_trip() {
            println!("{line}");
        }

        let mut socket = LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        let ack = socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        assert_eq!(ack.header & HEADER_TYPE_MASK, PACKAGE_TYPE_RESPONSE);
        assert_eq!(ack.frame.len(), RESPONSE_FIELDS + ack.declared_body_len);
        let profile: longbridge_proto::quote::UserQuoteProfileResponse = socket
            .request(
                QUERY_USER_QUOTE_PROFILE,
                &UserQuoteProfileRequest {
                    language: "en".to_owned(),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        let _ = profile;
        socket.shutdown().await;

        let stats = &socket.stats;
        println!("  every inbound frame of this socket, with its header byte:");
        for (millis, header, command, request_id, length) in &socket.trace {
            println!(
                "    +{millis} ms header={header:#04x} (type={} gzip={} verify={}) command={command} \
                 request_id={request_id:?} len={length}",
                header & HEADER_TYPE_MASK,
                header & HEADER_GZIP != 0,
                header & 0b0001_0000 != 0
            );
        }
        println!(
            "  inbound frames: {} response(s), {} push(es); frames whose length was not exactly \
             10 + body_len (response) or 5 + body_len (push): {}; gzip bit set: {}; verify bit set: \
             {}; frames this build decoded: {}",
            stats.responses,
            stats.pushes,
            stats.length_mismatches.len(),
            stats.gzip,
            stats.verify,
            socket.inbound_decoded
        );
        println!(
            "  §14.1 item 9 (does the venue gzip frames): measured gzip frames = {}",
            stats.gzip
        );
        println!(
            "  §14.1 item 10 (does the venue sign frames): measured verify frames = {}",
            stats.verify
        );
        socket.assert_framing(case);
        assert!(
            socket.inbound_decoded >= 2,
            "{case}: the acknowledgement and the profile answer are two inbound frames"
        );
        assert!(
            stats.responses >= 2,
            "{case}: the handshake and the profile are at least two responses"
        );
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 5 — AUTH: the acknowledgement's shape and its absolute expiry (§4.4).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_05_auth_acknowledges_with_an_absolute_millisecond_expiry() {
    let case = "case 5 (AUTH)";
    timeout(CASE_TIMEOUT, async {
        let credentials = skip_without_credentials!(case, Credentials::load());
        let mut socket =
            LiveSocket::connect(&credentials, chosen_transport(&credentials).await, true)
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));
        let ack = socket
            .authenticate()
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        let before = crate::grid::now_millis();
        let answer = longbridge_proto::control::AuthResponse::decode(ack.body.as_ref())
            .unwrap_or_else(|error| panic!("{case}: the AUTH body did not decode: {error}"));
        let after = crate::grid::now_millis();

        println!(
            "{case}: ack command_code={} status={:?} body_len={} session_id_len={} expires={} \
             limit={} online={}",
            ack.command_code,
            ack.status,
            ack.body.len(),
            answer.session_id.len(),
            answer.expires,
            answer.limit,
            answer.online
        );
        println!(
            "  now is between {before} and {after}; expires - now is between {} and {} ms \
             ({} h)",
            answer.expires - after,
            answer.expires - before,
            (answer.expires - before) / 3_600_000
        );

        assert_eq!(ack.command_code, 2, "{case}: the ack is cmd 2 (§4.4)");
        assert_eq!(ack.status, Some(0), "{case}: the ack's status is 0 (§4.4)");
        assert_eq!(
            ack.body.len(),
            ack.declared_body_len,
            "{case}: the ack's length field describes its body"
        );
        assert!(
            !answer.session_id.is_empty(),
            "{case}: the ack carries a session id (§4.4)"
        );
        assert!(
            answer.expires > after,
            "{case}: expires ({}) must be an absolute instant in the future, and now is {after}",
            answer.expires
        );
        assert!(
            answer.expires < after + 30 * 24 * 3_600_000,
            "{case}: expires ({}) is far beyond any window this suite knows; a relative \
             value read as absolute would land here",
            answer.expires
        );
        // The ack's own two counts are recorded, not asserted: the design does not use
        // them (there is no RECONNECT here), and the measurement below shows they are
        // not the same numbers the HTTP token endpoint reported.
        println!(
            "  the ack's limit/online are {} / {}; the token endpoint reported them as limit=10 \
             online=0 in case 1 — the two answers count different things",
            answer.limit, answer.online
        );
        socket.shutdown().await;
    })
    .await
    .expect("the case must finish inside its own bound");
}

/// Case 6 — the market-data profile: every rate limit, the market keys, and the US
/// entry's packages and warning (§4.4; §14.1 items 1 and 7).
#[tokio::test]
#[ignore = "requires live network access to the Longbridge OpenAPI"]
async fn live_case_06_the_profile_publishes_the_rate_limits_and_the_us_entry() {
    let case = "case 6 (cmd 4 profile)";
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
        let profile: longbridge_proto::quote::UserQuoteProfileResponse = socket
            .request(
                QUERY_USER_QUOTE_PROFILE,
                &UserQuoteProfileRequest {
                    language: "en".to_owned(),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));

        println!(
            "{case}: member_id={} quote_level={:?} subscribe_limit={} history_candlestick_limit={}",
            profile.member_id,
            profile.quote_level,
            profile.subscribe_limit,
            profile.history_candlestick_limit
        );
        println!("  rate_limit ({} entries):", profile.rate_limit.len());
        for rate in &profile.rate_limit {
            let command = longbridge_proto::quote::Command::try_from(rate.command)
                .map(|command| format!("{command:?}"))
                .unwrap_or_else(|_| "<unknown>".to_owned());
            println!(
                "    {{command: {}, limit: {}, burst: {}}}",
                rate.command, rate.limit, rate.burst
            );
            println!("      command {command}");
        }
        let detail = profile.quote_level_detail.as_ref();
        let mut markets: Vec<&String> = detail
            .map(|detail| detail.by_market_code.keys().collect())
            .unwrap_or_default();
        markets.sort();
        println!(
            "  by_market_code keys: {:?} (by_package_key has {} entries)",
            markets,
            detail
                .map(|detail| detail.by_package_key.len())
                .unwrap_or(0)
        );
        let us = detail.and_then(|detail| detail.by_market_code.get("US"));
        match us {
            Some(entry) => {
                println!(
                    "  US: packages={} warning_msg={:?}",
                    entry.packages.len(),
                    entry.warning_msg
                );
                for package in &entry.packages {
                    println!(
                        "    package key={:?} name={:?} start={} end={}",
                        package.key, package.name, package.start, package.end
                    );
                }
            }
            None => println!("  US: the profile states no US entry at all"),
        }
        socket.shutdown().await;

        assert_eq!(
            markets,
            vec!["CN", "HK", "US", "USOption"],
            "{case}: the venue's own four market codes (§4.4)"
        );
        assert!(
            !profile.rate_limit.is_empty(),
            "{case}: the profile states the account's per-command pace (§4.4)"
        );
        assert!(
            profile
                .rate_limit
                .iter()
                .all(|rate| rate.limit > 0 && rate.burst > 0),
            "{case}: every published pace states a positive limit and burst"
        );
        let us = us.expect("the profile states a US entry");
        if us.packages.is_empty() {
            assert!(
                !us.warning_msg.is_empty(),
                "{case}: an account without a market's package is warned about it (§4.4)"
            );
        }
    })
    .await
    .expect("the case must finish inside its own bound");
}
