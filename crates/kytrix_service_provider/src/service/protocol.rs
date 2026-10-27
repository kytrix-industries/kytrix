use std::pin::Pin;

use bytes::Bytes;
use futures::{Sink, SinkExt, future::poll_fn};
#[cfg(test)]
use kytrix_proto::client::{
    Request as ClientRequest, SetSubscriptions, request::Request as ClientRequestKind,
};
use kytrix_proto::{
    event as proto_event,
    server::{self as proto_response, ServerMessage, server_message},
    topic as proto_topic, types as proto_types,
};
use network::transport::websocket::RawWebSocket;
use prost::Message;
use smol_str::SmolStr;
use tokio::time::{Duration, timeout};
use url::Url;
use yawc::{Frame, OpCode, WebSocketError};

use crate::service::{
    ais::{AisArea, AisUpdate},
    cryptocurrency_wallet_monitoring,
    engine::{
        CryptocurrencyWalletMonitoringTransfer, ErrorCode, FinancialIndexEvent,
        FinancialIndexTopic, HEARTBEAT_MAX_SECONDS, HEARTBEAT_MIN_SECONDS, IncomingMessage,
        KytrixServiceError, KytrixServiceEvent, KytrixSessionState, NewsEvent, NewsKind,
        ServiceTopic, TweetEvent, TweetMedia, TweetReference, TweetTextSegment, WRITE_TIMEOUT,
        XProfileEvent,
    },
};

pub(super) async fn next_server_message(
    websocket: &mut RawWebSocket,
) -> Result<IncomingMessage, KytrixServiceError> {
    loop {
        let frame = websocket
            .next_frame()
            .await
            .map_err(|error| KytrixServiceError::Read(smol_str::format_smolstr!("{}", error)))?;
        match frame.opcode() {
            OpCode::Ping => {
                flush_protocol_frames(websocket, WRITE_TIMEOUT).await?;
            }
            OpCode::Pong => {
                return Ok(IncomingMessage::Pong(Bytes::copy_from_slice(
                    frame.payload(),
                )));
            }
            OpCode::Close => return Err(KytrixServiceError::Disconnected),
            OpCode::Text | OpCode::Continuation => {
                return Err(KytrixServiceError::Protocol(
                    "binary protobuf frames required".into(),
                ));
            }
            OpCode::Binary => return decode_binary(frame.payload()),
        }
    }
}

pub(super) fn decode_binary(payload: &[u8]) -> Result<IncomingMessage, KytrixServiceError> {
    let message = ServerMessage::decode(payload)
        .map_err(|error| KytrixServiceError::Protocol(smol_str::format_smolstr!("{}", error)))?;
    match message.message {
        Some(server_message::Message::Response(response)) => {
            Ok(IncomingMessage::Response(response))
        }
        Some(server_message::Message::Event(event)) => {
            Ok(IncomingMessage::Event(decode_event(event)?))
        }
        Some(server_message::Message::SessionState(state)) => {
            let plan_code = proto_types::PlanCode::try_from(state.plan_code).unwrap_or_else(|_| {
                log::warn!(
                    "unknown Kytrix service plan code {}; showing it as unspecified",
                    state.plan_code
                );
                proto_types::PlanCode::Unspecified
            });
            Ok(IncomingMessage::SessionState(KytrixSessionState {
                entitlement_revision: state.entitlement_revision,
                plan_code,
                entitlements: state.entitlements.into_iter().collect(),
                catalog: state.catalog,
                next_change_at: state.next_change_at,
            }))
        }
        Some(server_message::Message::AuthChallenge(challenge)) => {
            Ok(IncomingMessage::AuthChallenge(challenge))
        }
        None => Err(KytrixServiceError::Protocol(
            "server message oneof is required".into(),
        )),
    }
}

pub(super) fn decode_event(
    event: proto_event::Event,
) -> Result<Option<(ServiceTopic, KytrixServiceEvent)>, KytrixServiceError> {
    let Some(event) = event.event else {
        return Ok(None);
    };
    match event {
        proto_event::event::Event::News(news) => {
            let Some(category) = NewsKind::from_proto(news.kind) else {
                return Ok(None);
            };
            Ok(Some((
                ServiceTopic::News,
                KytrixServiceEvent::News(NewsEvent {
                    timestamp: news.timestamp,
                    title: news.title.map(Into::into),
                    content: news.content.into(),
                    category,
                }),
            )))
        }
        proto_event::event::Event::FinancialIndex(index) => {
            let Some(topic) = FinancialIndexTopic::from_code(&index.code) else {
                return Ok(None);
            };
            let service_topic = ServiceTopic::FinancialIndex(topic.clone());
            Ok(Some((
                service_topic,
                KytrixServiceEvent::FinancialIndex(FinancialIndexEvent {
                    topic,
                    last_price: index.last_price,
                    open_price: index.open_price,
                    high_price: index.high_price,
                    low_price: index.low_price,
                    close_price: index.close_price,
                    is_trading: index.is_trading,
                    timestamp: index.timestamp,
                }),
            )))
        }
        proto_event::event::Event::XProfile(profile) => Ok(Some((
            ServiceTopic::X,
            KytrixServiceEvent::XProfile(decode_x_profile(profile)?),
        ))),
        proto_event::event::Event::Tweet(tweet) => Ok(Some((
            ServiceTopic::X,
            KytrixServiceEvent::Tweet(decode_tweet(tweet)?),
        ))),
        proto_event::event::Event::CryptocurrencyWalletMonitoringTransfer(transfer) => Ok(Some((
            ServiceTopic::CryptocurrencyWalletMonitoring,
            KytrixServiceEvent::CryptocurrencyWalletMonitoringTransfer(
                decode_cryptocurrency_wallet_monitoring_transfer(transfer)?,
            ),
        ))),
        proto_event::event::Event::Ais(delta) => {
            let area = AisArea::try_from(delta.area)
                .ok()
                .filter(|area| *area != AisArea::Unspecified)
                .ok_or_else(|| KytrixServiceError::Protocol("AIS event area is invalid".into()))?;
            Ok(Some((
                ServiceTopic::Ais(area),
                KytrixServiceEvent::Ais(AisUpdate::from_proto(delta)),
            )))
        }
    }
}

pub(super) fn decode_cryptocurrency_wallet_monitoring_transfer(
    transfer: proto_types::CryptocurrencyWalletMonitoringTransfer,
) -> Result<CryptocurrencyWalletMonitoringTransfer, KytrixServiceError> {
    let network = cryptocurrency_wallet_monitoring::cryptocurrency_wallet_monitoring_network(
        &transfer.from_address,
    );
    if transfer.event_id.is_empty()
        || transfer.event_id.len() > 512
        || network.is_none()
        || cryptocurrency_wallet_monitoring::cryptocurrency_wallet_monitoring_network(
            &transfer.to_address,
        ) != network
        || !network.is_some_and(|network| {
            cryptocurrency_wallet_monitoring::valid_transfer_amount(&transfer.amount, network)
        })
        || transfer.token_symbol.is_empty()
        || transfer.token_symbol.len() > 64
        || transfer.timestamp <= 0
    {
        return Err(KytrixServiceError::Protocol(
            "cryptocurrency_wallet_monitoring transfer is invalid".into(),
        ));
    }
    let timestamp = transfer.timestamp.checked_mul(1_000).ok_or_else(|| {
        KytrixServiceError::Protocol(
            "cryptocurrency_wallet_monitoring timestamp exceeded UTC milliseconds".into(),
        )
    })?;
    Ok(CryptocurrencyWalletMonitoringTransfer {
        event_id: transfer.event_id.into(),
        from_address: transfer.from_address.into(),
        to_address: transfer.to_address.into(),
        amount: transfer.amount.into(),
        token_symbol: transfer.token_symbol.into(),
        timestamp,
    })
}

pub(super) fn decode_tweet(tweet: proto_types::Tweet) -> Result<TweetEvent, KytrixServiceError> {
    decode_tweet_at_depth(tweet, 0)
}

pub(super) fn decode_tweet_at_depth(
    tweet: proto_types::Tweet,
    depth: usize,
) -> Result<TweetEvent, KytrixServiceError> {
    let proto_types::Tweet {
        tweet_id,
        timestamp,
        text_segments,
        reference,
        x_user_id,
        media,
    } = tweet;
    if !valid_x_id(&tweet_id)
        || !valid_x_id(&x_user_id)
        || timestamp <= 0
        || text_segments.len() > 1_000
    {
        return Err(KytrixServiceError::Protocol(
            "Tweet contained an invalid ID, author ID, timestamp, or segment count".into(),
        ));
    }
    let mut text_bytes = 0_usize;
    let mut text_code_points = 0_usize;
    let text_segments = text_segments
        .into_iter()
        .map(|segment| {
            if segment.text.is_empty()
                || segment
                    .url
                    .as_deref()
                    .is_some_and(|url| !valid_http_url(url))
            {
                return Err(KytrixServiceError::Protocol(
                    "Tweet contained an invalid text segment".into(),
                ));
            }
            text_bytes = text_bytes
                .checked_add(segment.text.len())
                .ok_or_else(|| KytrixServiceError::Protocol("Tweet text size overflowed".into()))?;
            text_code_points = text_code_points
                .checked_add(segment.text.chars().count())
                .ok_or_else(|| KytrixServiceError::Protocol("Tweet text size overflowed".into()))?;
            Ok(TweetTextSegment {
                text: segment.text.into(),
                url: segment.url.map(Into::into),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if text_bytes > 100_000 || text_code_points > 25_000 {
        return Err(KytrixServiceError::Protocol(
            "Tweet contained too much text".into(),
        ));
    }
    let timestamp = timestamp.checked_mul(1_000).ok_or_else(|| {
        KytrixServiceError::Protocol("Tweet timestamp exceeded UTC milliseconds".into())
    })?;
    if depth >= 2 && reference.is_some() {
        return Err(KytrixServiceError::Protocol(
            "Tweet contained more than one nested reference".into(),
        ));
    }
    if media.len() > 4 {
        return Err(KytrixServiceError::Protocol(
            "Tweet contained too many media items".into(),
        ));
    }
    let media = media
        .into_iter()
        .map(decode_tweet_media)
        .collect::<Result<Vec<_>, _>>()?;
    let reference = reference
        .map(|reference| match reference {
            proto_types::tweet::Reference::Repost(tweet) => {
                decode_tweet_at_depth(*tweet, depth + 1)
                    .map(Box::new)
                    .map(TweetReference::Repost)
            }
            proto_types::tweet::Reference::Quote(tweet) => decode_tweet_at_depth(*tweet, depth + 1)
                .map(Box::new)
                .map(TweetReference::Quote),
        })
        .transpose()?;
    Ok(TweetEvent {
        tweet_id: tweet_id.into(),
        timestamp,
        x_user_id: x_user_id.into(),
        text_segments,
        reference,
        media,
    })
}

pub(super) fn decode_tweet_media(
    media: proto_types::TweetMedia,
) -> Result<TweetMedia, KytrixServiceError> {
    use proto_types::tweet_media::Content;

    let media = match media.content {
        Some(Content::Image(image))
            if valid_https_url(&image.image_url)
                && valid_media_dimensions(image.width, image.height) =>
        {
            TweetMedia::Image {
                image_url: image.image_url.into(),
                width: image.width,
                height: image.height,
            }
        }
        Some(Content::Video(video))
            if valid_https_url(&video.playback_url)
                && valid_https_url(&video.thumbnail_url)
                && valid_media_dimensions(video.width, video.height) =>
        {
            TweetMedia::Video {
                playback_url: video.playback_url.into(),
                thumbnail_url: video.thumbnail_url.into(),
                width: video.width,
                height: video.height,
            }
        }
        Some(Content::Gif(gif))
            if valid_https_url(&gif.animation_url)
                && valid_https_url(&gif.image_url)
                && valid_media_dimensions(gif.width, gif.height) =>
        {
            TweetMedia::Gif {
                animation_url: gif.animation_url.into(),
                image_url: gif.image_url.into(),
                width: gif.width,
                height: gif.height,
            }
        }
        _ => {
            return Err(KytrixServiceError::Protocol(
                "Tweet contained invalid media".into(),
            ));
        }
    };
    Ok(media)
}

pub(super) fn valid_https_url(value: &str) -> bool {
    value.len() <= 2_048
        && Url::parse(value).is_ok_and(|url| {
            url.scheme() == "https"
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
        })
}

pub(super) fn valid_http_url(value: &str) -> bool {
    value.len() <= 2_048
        && Url::parse(value).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
        })
}

const fn valid_media_dimensions(width: u32, height: u32) -> bool {
    (width == 0 && height == 0)
        || (width > 0
            && height > 0
            && width <= 16_384
            && height <= 16_384
            && (width as u64) * (height as u64) <= 100_000_000)
}

pub(super) fn decode_x_profile(
    profile: proto_types::XProfile,
) -> Result<XProfileEvent, KytrixServiceError> {
    if !valid_x_id(&profile.x_user_id)
        || !valid_x_username(&profile.username)
        || profile.display_name.is_empty()
        || profile.display_name.len() > 200
        || profile.display_name.chars().count() > 50
        || (!profile.avatar_url.is_empty() && !valid_https_url(&profile.avatar_url))
    {
        return Err(KytrixServiceError::Protocol(
            "X profile contained invalid fields".into(),
        ));
    }
    Ok(XProfileEvent {
        x_user_id: profile.x_user_id.into(),
        username: profile.username.into(),
        display_name: profile.display_name.into(),
        avatar_url: profile.avatar_url.into(),
        verified: profile.verified,
        revision: profile.revision,
    })
}

pub fn valid_x_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn valid_x_username(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 15
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub(super) fn validate_response(
    response: proto_response::Response,
    expected_id: u32,
    expected_topics: &[ServiceTopic],
) -> Result<(), KytrixServiceError> {
    let response = validate_response_envelope(response, expected_id)?;
    let topics = match response {
        proto_response::response::Response::SetSubscriptions(response) => response.topics,
        _ => {
            return Err(KytrixServiceError::Protocol(
                "unexpected response to set_subscriptions request".into(),
            ));
        }
    };
    if topics.len() != expected_topics.len() {
        return Err(KytrixServiceError::Protocol(smol_str::format_smolstr!(
            "response contained {} topics; expected {}",
            topics.len(),
            expected_topics.len()
        )));
    }
    let mut actual = topics
        .iter()
        .map(|topic| {
            ServiceTopic::try_from(topic).map_err(|error| {
                KytrixServiceError::Protocol(smol_str::format_smolstr!(
                    "invalid response topic: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    actual.sort_unstable();
    if actual != expected_topics {
        return Err(KytrixServiceError::Protocol(smol_str::format_smolstr!(
            "unexpected response topics {actual:?}; expected {expected_topics:?}"
        )));
    }
    Ok(())
}

pub(super) fn validate_rpc_response(
    response: proto_response::Response,
) -> Result<proto_response::Response, KytrixServiceError> {
    if response.id == 0 {
        return Err(KytrixServiceError::Protocol(
            "Kytrix service RPC response id is zero".into(),
        ));
    }
    let response_kind = response.response.as_ref().ok_or_else(|| {
        KytrixServiceError::Protocol("Kytrix service RPC response oneof is required".into())
    })?;
    if let proto_response::response::Response::Error(error) = response_kind {
        let code = ErrorCode::try_from(error.code)
            .map_err(|_| KytrixServiceError::Protocol("unknown service error code".into()))?;
        return Err(KytrixServiceError::Server(code));
    }
    Ok(response)
}

pub(super) fn validate_response_envelope(
    response: proto_response::Response,
    expected_id: u32,
) -> Result<proto_response::response::Response, KytrixServiceError> {
    if response.id != expected_id {
        return Err(KytrixServiceError::Protocol(smol_str::format_smolstr!(
            "unexpected response id {}; expected {expected_id}",
            response.id
        )));
    }
    let response = response
        .response
        .ok_or_else(|| KytrixServiceError::Protocol("server response oneof is required".into()))?;
    match response {
        proto_response::response::Response::Error(error) => {
            let code = ErrorCode::try_from(error.code).map_err(|_| {
                KytrixServiceError::Protocol(smol_str::format_smolstr!(
                    "invalid Kytrix service server error code {}",
                    error.code
                ))
            })?;
            Err(KytrixServiceError::Server(code))
        }
        response => Ok(response),
    }
}

pub(super) async fn send_frame_with_timeout(
    websocket: &mut RawWebSocket,
    frame: Frame,
    duration: Duration,
) -> Result<(), KytrixServiceError> {
    timeout(duration, websocket.send(frame))
        .await
        .map_err(|_| KytrixServiceError::Timeout)?
        .map_err(|error| KytrixServiceError::Write(smol_str::format_smolstr!("{}", error)))
}

pub(super) async fn flush_protocol_frames<S>(
    websocket: &mut S,
    duration: Duration,
) -> Result<(), KytrixServiceError>
where
    S: Sink<Frame, Error = WebSocketError> + Unpin,
{
    timeout(
        duration,
        poll_fn(|context| Pin::new(&mut *websocket).poll_ready(context)),
    )
    .await
    .map_err(|_| KytrixServiceError::Timeout)?
    .map_err(|error| KytrixServiceError::Write(smol_str::format_smolstr!("{}", error)))?;
    timeout(duration, websocket.flush())
        .await
        .map_err(|_| KytrixServiceError::Timeout)?
        .map_err(|error| KytrixServiceError::Write(smol_str::format_smolstr!("{}", error)))
}

pub(super) fn heartbeat_delay() -> Duration {
    Duration::from_secs(rand::random_range(
        HEARTBEAT_MIN_SECONDS..=HEARTBEAT_MAX_SECONDS,
    ))
}

impl NewsKind {
    fn from_proto(value: i32) -> Option<Self> {
        match proto_types::NewsKind::try_from(value).ok()? {
            proto_types::NewsKind::General => Some(Self::General),
            proto_types::NewsKind::Crypto => Some(Self::Crypto),
            proto_types::NewsKind::Political => Some(Self::Political),
            proto_types::NewsKind::NewsCategoryUnspecified => None,
        }
    }
}

impl From<&ServiceTopic> for proto_topic::Topic {
    fn from(topic: &ServiceTopic) -> Self {
        let topic = match topic {
            ServiceTopic::News => proto_topic::topic::Topic::News(proto_topic::NewsTopic {}),
            ServiceTopic::FinancialIndex(topic) => {
                proto_topic::topic::Topic::FinancialIndex(proto_topic::FinancialIndexTopic {
                    code: topic.code().into(),
                })
            }
            ServiceTopic::X => proto_topic::topic::Topic::X(proto_topic::TopicX {}),
            ServiceTopic::CryptocurrencyWalletMonitoring => {
                proto_topic::topic::Topic::CryptocurrencyWalletMonitoring(
                    proto_topic::CryptocurrencyWalletMonitoringTopic {},
                )
            }
            ServiceTopic::Ais(area) => {
                proto_topic::topic::Topic::Ais(proto_topic::AisTopic { area: *area as i32 })
            }
        };
        Self { topic: Some(topic) }
    }
}

impl TryFrom<&proto_topic::Topic> for ServiceTopic {
    type Error = SmolStr;

    fn try_from(topic: &proto_topic::Topic) -> Result<Self, Self::Error> {
        let Some(topic) = topic.topic.as_ref() else {
            return Err("topic oneof is required".into());
        };
        match topic {
            proto_topic::topic::Topic::News(_) => Ok(ServiceTopic::News),
            proto_topic::topic::Topic::FinancialIndex(value) => {
                FinancialIndexTopic::from_code(&value.code)
                    .map(ServiceTopic::FinancialIndex)
                    .ok_or_else(|| "index topic is invalid".into())
            }
            proto_topic::topic::Topic::X(_) => Ok(ServiceTopic::X),
            proto_topic::topic::Topic::CryptocurrencyWalletMonitoring(_) => {
                Ok(ServiceTopic::CryptocurrencyWalletMonitoring)
            }
            proto_topic::topic::Topic::Ais(value) => match AisArea::try_from(value.area) {
                Ok(AisArea::Hormuz) => Ok(ServiceTopic::Ais(AisArea::Hormuz)),
                Ok(AisArea::Unspecified) => Err("AIS area is unspecified".into()),
                Err(_) => Err("AIS area is invalid".into()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{super::test_support::*, *};
    use crate::service::ais::{AisPosition, AisStatic};

    #[test]
    fn literal_set_subscriptions_wire_should_preserve_schema_tags() {
        let request = ClientRequest::decode(SET_SUBSCRIPTIONS_WIRE)
            .expect("literal subscription request should decode");
        assert_eq!(request.id, 7);
        let Some(ClientRequestKind::SetSubscriptions(subscriptions)) = request.request else {
            panic!("literal request should contain set_subscriptions");
        };
        assert_eq!(
            subscriptions
                .topics
                .iter()
                .map(ServiceTopic::try_from)
                .collect::<Result<Vec<_>, _>>(),
            Ok(vec![ServiceTopic::News, index_service_topic("SPX")])
        );

        let outbound = ClientRequest {
            id: 7,
            request: Some(ClientRequestKind::SetSubscriptions(SetSubscriptions {
                topics: vec![
                    proto_topic::Topic::from(&ServiceTopic::News),
                    proto_topic::Topic::from(&index_service_topic("SPX")),
                ],
            })),
        };
        assert_eq!(outbound.encode_to_vec(), SET_SUBSCRIPTIONS_WIRE);
    }

    #[test]
    fn literal_news_event_wire_should_preserve_schema_tags() {
        let IncomingMessage::Event(Some((ServiceTopic::News, KytrixServiceEvent::News(news)))) =
            decode_binary(NEWS_EVENT_WIRE).expect("literal News event should decode")
        else {
            panic!("literal frame should contain a News event");
        };
        assert_eq!(news.timestamp, 1_234);
        assert_eq!(news.title.as_deref(), Some("T"));
        assert_eq!(news.content, "C");
        assert_eq!(news.category, NewsKind::Crypto);
    }

    #[test]
    fn literal_index_event_wire_should_preserve_schema_tags() {
        let IncomingMessage::Event(Some((
            ServiceTopic::FinancialIndex(topic),
            KytrixServiceEvent::FinancialIndex(index),
        ))) = decode_binary(INDEX_EVENT_WIRE).expect("literal Index event should decode")
        else {
            panic!("literal frame should contain an Index event");
        };
        assert_eq!(topic, financial_index_topic("SPX"));
        assert_eq!(index.topic, topic);
        assert_eq!(
            (
                index.last_price,
                index.open_price,
                index.high_price,
                index.low_price,
                index.close_price,
                index.is_trading,
                index.timestamp,
            ),
            (101, 90, 110, 80, 95, true, 1_234)
        );
    }

    #[test]
    fn literal_x_event_wire_should_preserve_schema_tags() {
        let IncomingMessage::Event(Some((ServiceTopic::X, KytrixServiceEvent::XProfile(profile)))) =
            decode_binary(X_PROFILE_EVENT_WIRE).expect("literal X profile event should decode")
        else {
            panic!("literal frame should contain an X profile event");
        };
        assert_eq!(profile.x_user_id, "42");
        assert_eq!(profile.username, "ky");
        assert_eq!(profile.display_name, "Ky");
        assert_eq!(profile.avatar_url, "https://x.test/a");
        assert!(profile.verified);
        assert_eq!(profile.revision, 7);
    }

    #[test]
    fn literal_cryptocurrency_wallet_monitoring_event_wire_should_preserve_schema_tags() {
        let IncomingMessage::Event(Some((
            ServiceTopic::CryptocurrencyWalletMonitoring,
            KytrixServiceEvent::CryptocurrencyWalletMonitoringTransfer(transfer),
        ))) = decode_binary(CRYPTOCURRENCY_WALLET_MONITORING_EVENT_WIRE)
            .expect("literal CryptocurrencyWalletMonitoring event should decode")
        else {
            panic!("literal frame should contain a CryptocurrencyWalletMonitoring event");
        };
        assert_eq!(transfer.event_id, "e");
        assert_eq!(transfer.from_address, "11111111111111111111111111111111");
        assert_eq!(transfer.to_address, transfer.from_address);
        assert_eq!(transfer.amount, "1.5");
        assert_eq!(transfer.token_symbol, "SOL");
        assert_eq!(transfer.timestamp, 1_234_000);
    }

    #[test]
    fn literal_unknown_server_oneof_should_be_a_protocol_error() {
        assert!(matches!(
            decode_binary(UNKNOWN_SERVER_ONEOF_WIRE),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn topic_should_round_trip_through_protobuf() {
        for topic in [
            ServiceTopic::News,
            index_service_topic("REMOTE_ONLY_INDEX"),
            ServiceTopic::X,
            ServiceTopic::CryptocurrencyWalletMonitoring,
            ServiceTopic::Ais(AisArea::Hormuz),
        ] {
            let encoded = proto_topic::Topic::from(&topic);
            assert_eq!(ServiceTopic::try_from(&encoded), Ok(topic));
        }
    }

    #[test]
    fn ais_topic_should_reject_unspecified_and_unknown_areas() {
        for area in [AisArea::Unspecified as i32, 9] {
            let topic = proto_topic::Topic {
                topic: Some(proto_topic::topic::Topic::Ais(proto_topic::AisTopic {
                    area,
                })),
            };
            assert!(ServiceTopic::try_from(&topic).is_err());
        }
    }

    #[test]
    fn invalid_index_event_codes_should_not_be_delivered() {
        for code in ["", " INVALID", "INVALID "] {
            let message = ServerMessage {
                message: Some(server_message::Message::Event(proto_event::Event {
                    event: Some(proto_event::event::Event::FinancialIndex(
                        proto_types::FinancialIndex {
                            code: code.to_owned(),
                            ..Default::default()
                        },
                    )),
                })),
            };
            let decoded = decode_binary(&message.encode_to_vec()).expect("event should decode");
            assert!(matches!(decoded, IncomingMessage::Event(None)));
        }
    }

    #[test]
    fn invalid_index_topic_in_a_response_should_be_a_protocol_error() {
        let response = proto_response::Response {
            id: 7,
            response: Some(proto_response::response::Response::SetSubscriptions(
                proto_response::SetSubscriptionsResponse {
                    topics: vec![proto_topic::Topic {
                        topic: Some(proto_topic::topic::Topic::FinancialIndex(
                            proto_topic::FinancialIndexTopic {
                                code: String::new(),
                            },
                        )),
                    }],
                },
            )),
        };

        assert!(matches!(
            validate_response(response, 7, &[index_service_topic("DJI")]),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn set_subscriptions_request_should_encode_a_topic_batch_and_nonzero_id() {
        let topics = vec![ServiceTopic::News, index_service_topic("SPX")];
        let request = ClientRequest {
            id: 7,
            request: Some(ClientRequestKind::SetSubscriptions(SetSubscriptions {
                topics: topics.iter().map(Into::into).collect(),
            })),
        };
        let encoded = request.encode_to_vec();
        let decoded = ClientRequest::decode(encoded.as_slice()).expect("request should decode");
        assert_eq!(decoded.id, 7);
        let Some(ClientRequestKind::SetSubscriptions(set)) = decoded.request else {
            panic!("expected a set_subscriptions request");
        };
        assert_eq!(
            set.topics
                .iter()
                .map(ServiceTopic::try_from)
                .collect::<Result<Vec<_>, _>>(),
            Ok(topics)
        );
    }

    #[test]
    fn all_news_kinds_should_decode_to_the_same_topic() {
        for (wire_kind, kind) in [
            (proto_types::NewsKind::General, NewsKind::General),
            (proto_types::NewsKind::Crypto, NewsKind::Crypto),
            (proto_types::NewsKind::Political, NewsKind::Political),
        ] {
            let message = ServerMessage {
                message: Some(server_message::Message::Event(proto_event::Event {
                    event: Some(proto_event::event::Event::News(proto_types::News {
                        timestamp: 12,
                        title: Some("title".to_owned()),
                        content: "content".to_owned(),
                        kind: wire_kind as i32,
                    })),
                })),
            };
            let decoded = decode_binary(&message.encode_to_vec()).expect("event should decode");
            let IncomingMessage::Event(Some((topic, KytrixServiceEvent::News(news)))) = decoded
            else {
                panic!("expected a news event");
            };
            assert_eq!(topic, ServiceTopic::News);
            assert_eq!(news.category, kind);
        }
    }

    #[test]
    fn tweet_event_should_decode_milliseconds_and_one_repost() {
        let mut root = tweet("100", 1_700_000_000);
        let mut nested = tweet("99", 1_699_999_999);
        nested.text_segments = vec![proto_types::TweetTextSegment {
            text: "example.test/nested".to_owned(),
            url: Some("https://t.co/nested".to_owned()),
        }];
        root.reference = Some(proto_types::tweet::Reference::Repost(Box::new(nested)));
        let message = ServerMessage {
            message: Some(server_message::Message::Event(proto_event::Event {
                event: Some(proto_event::event::Event::Tweet(root)),
            })),
        };

        let decoded = decode_binary(&message.encode_to_vec()).expect("Tweet should decode");
        let IncomingMessage::Event(Some((ServiceTopic::X, KytrixServiceEvent::Tweet(tweet)))) =
            decoded
        else {
            panic!("expected a Tweet event");
        };
        assert_eq!(tweet.tweet_id, "100");
        assert_eq!(tweet.timestamp, 1_700_000_000_000);
        assert_eq!(tweet.x_user_id, "42");
        assert_eq!(
            tweet.text_segments,
            [TweetTextSegment {
                text: "hello".into(),
                url: None,
            }]
        );
        let TweetReference::Repost(repost) = tweet.reference.expect("repost should be retained")
        else {
            panic!("expected a repost");
        };
        assert_eq!(repost.tweet_id, "99");
        assert_eq!(repost.timestamp, 1_699_999_999_000);
        assert_eq!(
            repost.text_segments,
            [TweetTextSegment {
                text: "example.test/nested".into(),
                url: Some("https://t.co/nested".into()),
            }]
        );
    }

    #[test]
    fn x_profile_event_should_decode_verified_state() {
        let message = ServerMessage {
            message: Some(server_message::Message::Event(proto_event::Event {
                event: Some(proto_event::event::Event::XProfile(x_profile())),
            })),
        };

        let decoded = decode_binary(&message.encode_to_vec()).unwrap();
        let IncomingMessage::Event(Some((ServiceTopic::X, KytrixServiceEvent::XProfile(profile)))) =
            decoded
        else {
            panic!("expected an X profile event");
        };
        assert_eq!(profile.username, "kytrix");
        assert!(profile.verified);
    }

    #[test]
    fn cryptocurrency_wallet_monitoring_transfer_should_decode_exact_amount_and_milliseconds() {
        let message = ServerMessage {
            message: Some(server_message::Message::Event(proto_event::Event {
                event: Some(
                    proto_event::event::Event::CryptocurrencyWalletMonitoringTransfer(
                        proto_types::CryptocurrencyWalletMonitoringTransfer {
                            event_id: "ethereum:0xabc:2".to_owned(),
                            from_address: "0x0000000000000000000000000000000000000001".to_owned(),
                            to_address: "0x0000000000000000000000000000000000000002".to_owned(),
                            amount: "1.000001".to_owned(),
                            token_symbol: "USDC".to_owned(),
                            timestamp: 1_786_665_600,
                        },
                    ),
                ),
            })),
        };

        let decoded = decode_binary(&message.encode_to_vec()).unwrap();
        let IncomingMessage::Event(Some((
            ServiceTopic::CryptocurrencyWalletMonitoring,
            KytrixServiceEvent::CryptocurrencyWalletMonitoringTransfer(transfer),
        ))) = decoded
        else {
            panic!("expected a cryptocurrency_wallet_monitoring transfer");
        };
        assert_eq!(transfer.event_id, "ethereum:0xabc:2");
        assert_eq!(transfer.amount, "1.000001");
        assert_eq!(transfer.token_symbol, "USDC");
        assert_eq!(transfer.timestamp, 1_786_665_600_000);
    }

    #[test]
    fn cryptocurrency_wallet_monitoring_transfer_should_decode_zero_amount() {
        let message = ServerMessage {
            message: Some(server_message::Message::Event(proto_event::Event {
                event: Some(
                    proto_event::event::Event::CryptocurrencyWalletMonitoringTransfer(
                        proto_types::CryptocurrencyWalletMonitoringTransfer {
                            event_id: "ethereum:0xabc:3".to_owned(),
                            from_address: "0x0000000000000000000000000000000000000001".to_owned(),
                            to_address: "0x0000000000000000000000000000000000000002".to_owned(),
                            amount: "0".to_owned(),
                            token_symbol: "ETH".to_owned(),
                            timestamp: 1_786_665_600,
                        },
                    ),
                ),
            })),
        };

        let decoded = decode_binary(&message.encode_to_vec()).unwrap();
        let IncomingMessage::Event(Some((
            ServiceTopic::CryptocurrencyWalletMonitoring,
            KytrixServiceEvent::CryptocurrencyWalletMonitoringTransfer(transfer),
        ))) = decoded
        else {
            panic!("expected a cryptocurrency_wallet_monitoring transfer");
        };
        assert_eq!(transfer.amount, "0");
    }

    #[test]
    fn ais_delta_should_decode_the_area_and_every_part_of_the_update() {
        let message = ServerMessage {
            message: Some(server_message::Message::Event(proto_event::Event {
                event: Some(proto_event::event::Event::Ais(ais_delta())),
            })),
        };

        let decoded = decode_binary(&message.encode_to_vec()).unwrap();
        let IncomingMessage::Event(Some((
            ServiceTopic::Ais(AisArea::Hormuz),
            KytrixServiceEvent::Ais(update),
        ))) = decoded
        else {
            panic!("expected an AIS update");
        };
        assert_eq!(update, AisUpdate::from_proto(ais_delta()));
        assert_eq!(update.epoch, 5);
        assert!(!update.keyframe);
        assert_eq!(update.sequence, 7);
        assert_eq!(update.base, 6);
        assert_eq!(
            update.statics,
            [AisStatic {
                mmsi: 636_000_001,
                imo: 9_000_001,
                call_sign: "C6AB1".into(),
                name: "HORMUZ STAR".into(),
                destination: "FUJAIRAH".into(),
                ship_type: 80,
                flag_mid: 636,
                draught: 123,
                dim_to_bow: 100,
                dim_to_stern: 20,
                dim_to_port: 10,
                dim_to_starboard: 11,
                eta_month: 6,
                eta_day: 15,
                eta_hour: 12,
                eta_minute: 30,
            }]
        );
        assert_eq!(
            update.positions,
            [AisPosition {
                mmsi: 636_000_001,
                latitude: 26_566_000,
                longitude: 56_250_000,
                sog: 123,
                cog: 456,
                heading: 47,
                nav_status: 0,
                last_update: 1_700_000_000_000,
            }]
        );
        assert_eq!(update.removed, [636_000_002]);
        assert_eq!(update.timestamp, 1_700_000_000_000);
    }

    #[test]
    fn ais_event_without_a_known_area_should_be_a_protocol_error() {
        for area in [AisArea::Unspecified as i32, 9] {
            let mut delta = ais_delta();
            delta.area = area;
            let message = ServerMessage {
                message: Some(server_message::Message::Event(proto_event::Event {
                    event: Some(proto_event::event::Event::Ais(delta)),
                })),
            };
            assert!(matches!(
                decode_binary(&message.encode_to_vec()),
                Err(KytrixServiceError::Protocol(_))
            ));
        }
    }

    #[test]
    fn invalid_tweet_ids_timestamps_and_reference_depth_should_be_protocol_errors() {
        let mut invalid_id = tweet("bad", 1);
        let mut invalid_timestamp = tweet("1", 0);
        let mut missing_author_id = tweet("1", 1);
        missing_author_id.x_user_id.clear();
        let mut two_references = tweet("1", 1);
        let mut nested = tweet("2", 1);
        nested.reference = Some(proto_types::tweet::Reference::Quote(Box::new(tweet(
            "3", 1,
        ))));
        two_references.reference = Some(proto_types::tweet::Reference::Repost(Box::new(nested)));
        assert!(decode_tweet(two_references.clone()).is_ok());
        let proto_types::tweet::Reference::Repost(nested) =
            two_references.reference.as_mut().unwrap()
        else {
            unreachable!()
        };
        let proto_types::tweet::Reference::Quote(leaf) = nested.reference.as_mut().unwrap() else {
            unreachable!()
        };
        leaf.reference = Some(proto_types::tweet::Reference::Quote(Box::new(tweet(
            "4", 1,
        ))));
        invalid_id.timestamp = 1;
        invalid_timestamp.tweet_id = "1".to_owned();

        for value in [
            invalid_id,
            invalid_timestamp,
            missing_author_id,
            two_references,
            tweet("1", i64::MAX),
        ] {
            assert!(matches!(
                decode_tweet(value),
                Err(KytrixServiceError::Protocol(_))
            ));
        }
    }

    #[test]
    fn invalid_tweet_text_segments_should_be_protocol_errors() {
        let mut empty = tweet("1", 1);
        empty.text_segments[0].text.clear();
        let mut invalid_url = tweet("1", 1);
        invalid_url.text_segments[0].url = Some("javascript:alert(1)".to_owned());
        let mut overlong = tweet("1", 1);
        overlong.text_segments[0].text = "x".repeat(100_001);

        for value in [empty, invalid_url, overlong] {
            assert!(matches!(
                decode_tweet(value),
                Err(KytrixServiceError::Protocol(_))
            ));
        }
    }

    #[test]
    fn server_envelope_should_decode_session_state() {
        let catalog = feature_definition("news", proto_types::FeatureValueType::Bool);
        let message = ServerMessage {
            message: Some(server_message::Message::SessionState(
                proto_response::SessionState {
                    entitlement_revision: 7,
                    plan_code: proto_types::PlanCode::Plus as i32,
                    entitlements: std::collections::HashMap::from([
                        ("news".to_owned(), enabled_entitlement(true)),
                        ("financial_indices".to_owned(), limit_entitlement(3)),
                    ]),
                    catalog: vec![catalog.clone()],
                    next_change_at: Some(1_800_000_000),
                },
            )),
        };

        let decoded = decode_binary(&message.encode_to_vec()).expect("session state should decode");
        let IncomingMessage::SessionState(state) = decoded else {
            panic!("expected a session state");
        };
        assert_eq!(state.entitlement_revision, 7);
        assert_eq!(state.plan_code, proto_types::PlanCode::Plus);
        assert_eq!(
            state.entitlements,
            BTreeMap::from([
                ("financial_indices".to_owned(), limit_entitlement(3)),
                ("news".to_owned(), enabled_entitlement(true)),
            ])
        );
        assert_eq!(state.catalog, vec![catalog]);
        assert_eq!(state.next_change_at, Some(1_800_000_000));
    }

    #[test]
    fn an_unknown_plan_code_should_decode_as_unspecified() {
        let message = ServerMessage {
            message: Some(server_message::Message::SessionState(
                proto_response::SessionState {
                    entitlement_revision: 7,
                    plan_code: 99,
                    entitlements: std::collections::HashMap::new(),
                    catalog: Vec::new(),
                    next_change_at: None,
                },
            )),
        };

        let decoded = decode_binary(&message.encode_to_vec()).expect("session state should decode");
        let IncomingMessage::SessionState(state) = decoded else {
            panic!("expected a session state");
        };
        assert_eq!(state.plan_code, proto_types::PlanCode::Unspecified);
    }

    #[test]
    fn server_envelope_should_decode_authentication_challenge() {
        let message = ServerMessage {
            message: Some(server_message::Message::AuthChallenge(
                proto_response::AuthChallenge {
                    nonce: "challenge-nonce".to_owned(),
                    expires_at: 1_800_000_000,
                },
            )),
        };

        let decoded = decode_binary(&message.encode_to_vec()).expect("challenge should decode");

        assert!(matches!(
            decoded,
            IncomingMessage::AuthChallenge(proto_response::AuthChallenge {
                nonce,
                expires_at: 1_800_000_000,
            }) if nonce == "challenge-nonce"
        ));
    }

    #[test]
    fn unknown_event_variant_should_be_ignored() {
        let message = ServerMessage {
            message: Some(server_message::Message::Event(proto_event::Event {
                event: None,
            })),
        };
        let decoded = decode_binary(&message.encode_to_vec()).expect("unknown event should decode");
        assert!(matches!(decoded, IncomingMessage::Event(None)));
    }

    #[test]
    fn malformed_server_envelope_should_request_reconnect() {
        assert!(matches!(
            decode_binary(&[0x0a, 0x01, 0xff]),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn index_event_should_decode_all_wire_prices() {
        let message = ServerMessage {
            message: Some(server_message::Message::Event(proto_event::Event {
                event: Some(proto_event::event::Event::FinancialIndex(
                    proto_types::FinancialIndex {
                        code: "REMOTE_ONLY_INDEX".to_owned(),
                        last_price: 101,
                        open_price: 77,
                        high_price: 202,
                        low_price: 33,
                        close_price: 88,
                        is_trading: false,
                        timestamp: 1_234,
                    },
                )),
            })),
        };
        let decoded = decode_binary(&message.encode_to_vec()).expect("index should decode");
        let IncomingMessage::Event(Some((topic, KytrixServiceEvent::FinancialIndex(index)))) =
            decoded
        else {
            panic!("expected an index event");
        };
        assert_eq!(topic, index_service_topic("REMOTE_ONLY_INDEX"));
        assert_eq!(index.last_price, 101);
        assert_eq!(index.open_price, 77);
        assert_eq!(index.high_price, 202);
        assert_eq!(index.low_price, 33);
        assert_eq!(index.close_price, 88);
        assert!(!index.is_trading);
        assert_eq!(index.timestamp, 1_234);
    }

    #[test]
    fn response_validation_should_reject_wrong_id_and_topic() {
        let response = proto_response::Response {
            id: 8,
            response: Some(proto_response::response::Response::SetSubscriptions(
                proto_response::SetSubscriptionsResponse {
                    topics: vec![proto_topic::Topic::from(&ServiceTopic::News)],
                },
            )),
        };
        assert!(matches!(
            validate_response(response.clone(), 7, &[ServiceTopic::News]),
            Err(KytrixServiceError::Protocol(_))
        ));
        assert!(matches!(
            validate_response(
                proto_response::Response { id: 7, ..response },
                7,
                &[index_service_topic("DJI")]
            ),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn response_validation_should_accept_the_complete_topic_set_in_any_order() {
        let expected = vec![ServiceTopic::News, index_service_topic("SPX")];
        let response = control_response(7, &[expected[1].clone(), expected[0].clone()]);

        assert_eq!(validate_response(response, 7, &expected), Ok(()));
    }

    #[test]
    fn response_validation_should_reject_missing_topics() {
        let expected = vec![ServiceTopic::News, index_service_topic("SPX")];
        let response = control_response(7, &expected[..1]);

        assert!(matches!(
            validate_response(response, 7, &expected),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn response_validation_should_reject_extra_topics() {
        let expected = vec![ServiceTopic::News];
        let response = control_response(7, &[ServiceTopic::News, index_service_topic("SPX")]);

        assert!(matches!(
            validate_response(response, 7, &expected),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn response_validation_should_reject_duplicate_topics() {
        let expected = vec![ServiceTopic::News, index_service_topic("SPX")];
        let response = control_response(7, &[ServiceTopic::News, ServiceTopic::News]);

        assert!(matches!(
            validate_response(response, 7, &expected),
            Err(KytrixServiceError::Protocol(_))
        ));
    }

    #[test]
    fn set_subscriptions_error_response_should_map_to_typed_server_error() {
        let error = validate_response(
            error_response(7, ErrorCode::RateLimit as i32),
            7,
            &[ServiceTopic::News],
        )
        .expect_err("server error should fail set_subscriptions");

        assert_eq!(error, KytrixServiceError::Server(ErrorCode::RateLimit));
    }

    #[test]
    fn server_error_text_should_name_known_codes() {
        assert_eq!(
            (
                KytrixServiceError::Server(ErrorCode::RateLimit).to_string(),
                KytrixServiceError::Server(ErrorCode::Unspecified).to_string(),
            ),
            (
                "Kytrix service server error: RATE_LIMIT".to_owned(),
                "Kytrix service server error: UNSPECIFIED".to_owned(),
            )
        );
    }

    #[test]
    fn unknown_server_error_code_should_be_a_protocol_error() {
        assert!(matches!(
            validate_response(error_response(7, 999), 7, &[]),
            Err(KytrixServiceError::Protocol(_))
        ));
    }
}
