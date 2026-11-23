use std::sync::Arc;

use bytes::Bytes;
use reqwest::header::{ACCEPT_LANGUAGE, HeaderMap, HeaderName, HeaderValue};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use url::Url;
use yawc::{Frame, OpCode};

use crate::{
    bounds::{ANSWER_TIMEOUT, CONNECT_TIMEOUT, MAX_READ, REQUEST_TIMEOUT_MILLIS},
    client::{proxy_for, quote_endpoint},
    credentials::Credentials,
    http::fetch_otp,
    wire::{COMMAND_CODE_AUTH, Failure, FrameStats, Inbound, LOGIN_REFUSAL, refusal},
};

pub struct LiveSocket {
    socket: network::transport::websocket::RawWebSocket,
    hub: Arc<network::transport::hub::TransportHub>,
    pub endpoint: Url,
    otp: String,
    next_request_id: u32,
    pub stats: FrameStats,
    pub trace: Vec<(u128, u8, u8, Option<u32>, usize)>,
    opened: tokio::time::Instant,
    pushes: Vec<Inbound>,
    pub inbound_decoded: usize,
    pub commands_sent: Vec<u8>,
}

impl LiveSocket {
    pub async fn connect(
        credentials: &Credentials,
        transport: crate::client::Transport,
        with_region_header: bool,
    ) -> Result<Self, Failure> {
        let hub = Arc::new(network::transport::hub::TransportHub::new(
            network::transport::hub::TransportPolicy::default(),
        ));
        let client = crate::client::live_http_client(transport)
            .map_err(|error| Failure::Transport(error.to_string()))?;
        let otp = fetch_otp(&client, credentials, with_region_header).await?;
        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en"));
        if with_region_header {
            headers.insert(
                HeaderName::from_static("x-dc-region"),
                HeaderValue::from_static(credentials.data_centre()),
            );
        }
        let endpoint = quote_endpoint().map_err(|error| Failure::Transport(error.to_string()))?;
        let proxy = proxy_for(transport);
        let socket = timeout(
            CONNECT_TIMEOUT,
            hub.connect_websocket(
                &endpoint,
                &proxy,
                headers.clone(),
                &CancellationToken::new(),
            ),
        )
        .await
        .map_err(|_| Failure::Transport("the upgrade timed out".to_owned()))?
        .map_err(|error| Failure::Transport(error.to_string()))?;
        Ok(Self {
            socket,
            hub,
            endpoint,
            otp,
            next_request_id: 0,
            stats: FrameStats::default(),
            trace: Vec::with_capacity(16),
            opened: tokio::time::Instant::now(),
            pushes: Vec::with_capacity(8),
            inbound_decoded: 0,
            commands_sent: Vec::with_capacity(8),
        })
    }

    pub async fn authenticate(&mut self) -> Result<Inbound, Failure> {
        let body = longbridge_proto::control::AuthRequest {
            token: self.otp.clone(),
            metadata: [
                ("accept-language".to_owned(), "en".to_owned()),
                ("need_over_night_quote".to_owned(), "true".to_owned()),
            ]
            .into_iter()
            .collect(),
        };
        let frame = crate::wire::encode_request(
            COMMAND_CODE_AUTH,
            crate::bounds::AUTH_REQUEST_ID,
            REQUEST_TIMEOUT_MILLIS,
            &prost::Message::encode_to_vec(&body),
        );
        self.send_raw(frame, COMMAND_CODE_AUTH).await?;
        let deadline = tokio::time::Instant::now() + ANSWER_TIMEOUT;
        loop {
            let answer = self.read_frame(deadline).await?;
            if answer.command_code != COMMAND_CODE_AUTH {
                continue;
            }
            if answer.status != Some(0) {
                let (code, message) = refusal(&answer.body);
                return Err(Failure::Fact(format!(
                    "{LOGIN_REFUSAL}: code {code}, message {message}"
                )));
            }
            return Ok(answer);
        }
    }

    pub async fn request_raw(&mut self, command_code: u8, body: &[u8]) -> Result<Inbound, Failure> {
        self.next_request_id += 1;
        let request_id = self.next_request_id;
        let frame =
            crate::wire::encode_request(command_code, request_id, REQUEST_TIMEOUT_MILLIS, body);
        self.send_raw(frame, command_code).await?;
        let deadline = tokio::time::Instant::now() + ANSWER_TIMEOUT;
        loop {
            let answer = self.read_frame(deadline).await?;
            if answer.request_id != Some(request_id) {
                continue;
            }
            if let Some(status) = answer.status
                && status != 0
            {
                let (code, message) = refusal(&answer.body);
                return Err(Failure::Refused {
                    command: command_code,
                    code,
                    message,
                });
            }
            return Ok(answer);
        }
    }

    pub async fn request<M: prost::Message + Default>(
        &mut self,
        command_code: u8,
        body: &impl prost::Message,
    ) -> Result<M, Failure> {
        let answer = self
            .request_raw(command_code, &body.encode_to_vec())
            .await?;
        M::decode(answer.body.as_ref())
            .map_err(|error| Failure::Malformed(format!("command {command_code}: {error}")))
    }

    async fn send_raw(&mut self, frame: Bytes, command: u8) -> Result<(), Failure> {
        self.commands_sent.push(command);
        self.socket
            .send(Frame::binary(frame))
            .await
            .map_err(|error| Failure::Transport(error.to_string()))
    }

    async fn read_frame(&mut self, deadline: tokio::time::Instant) -> Result<Inbound, Failure> {
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let frame = timeout(remaining, self.socket.next_frame())
                .await
                .map_err(|_| Failure::Transport("the venue stopped answering".to_owned()))?
                .map_err(|error| Failure::Transport(error.to_string()))?;
            match frame.opcode() {
                OpCode::Binary => {
                    let payload = Bytes::copy_from_slice(frame.payload());
                    let decoded = crate::wire::decode_inbound(payload, &mut self.stats);
                    if decoded.is_ok() {
                        self.inbound_decoded += 1;
                    }
                    let decoded = decoded?;
                    self.trace.push((
                        self.opened.elapsed().as_millis(),
                        decoded.header,
                        decoded.command_code,
                        decoded.request_id,
                        decoded.frame.len(),
                    ));
                    if decoded.request_id.is_none() {
                        self.pushes.push(decoded.clone());
                    }
                    return Ok(decoded);
                }
                OpCode::Close => {
                    return Err(Failure::Transport("the venue closed the socket".to_owned()));
                }
                _ => continue,
            }
        }
    }

    pub async fn drain_pushes(&mut self, until: tokio::time::Instant) -> Vec<Inbound> {
        while self.read_frame_raw(until).await.is_ok() {}
        std::mem::take(&mut self.pushes)
    }

    async fn read_frame_raw(&mut self, deadline: tokio::time::Instant) -> Result<(), Failure> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let bound = if remaining.is_zero() {
            MAX_READ
        } else {
            remaining
        };
        let frame = timeout(bound, self.socket.next_frame())
            .await
            .map_err(|_| Failure::Transport("the venue stopped answering".to_owned()))?
            .map_err(|error| Failure::Transport(error.to_string()))?;
        match frame.opcode() {
            OpCode::Binary => {
                let payload = Bytes::copy_from_slice(frame.payload());
                let decoded = crate::wire::decode_inbound(payload, &mut self.stats)?;
                self.inbound_decoded += 1;
                self.trace.push((
                    self.opened.elapsed().as_millis(),
                    decoded.header,
                    decoded.command_code,
                    decoded.request_id,
                    decoded.frame.len(),
                ));
                if decoded.request_id.is_none() {
                    self.pushes.push(decoded);
                }
                Ok(())
            }
            OpCode::Close => Err(Failure::Transport("the venue closed the socket".to_owned())),
            _ => Ok(()),
        }
    }

    pub async fn shutdown(&self) {
        self.hub.shutdown().await;
    }

    pub fn assert_framing(&self, case: &str) {
        assert!(
            self.stats.length_mismatches.is_empty(),
            "{case}: every inbound frame must be exactly 10 + body_len bytes for a response \
             and 5 + body_len for a push; {} did not: {:?}",
            self.stats.length_mismatches.len(),
            self.stats.length_mismatches
        );
        assert!(
            self.stats.unknown_types.is_empty(),
            "{case}: every inbound frame must carry packet type 2 or 3; {:?} did not",
            self.stats.unknown_types
        );
    }
}
