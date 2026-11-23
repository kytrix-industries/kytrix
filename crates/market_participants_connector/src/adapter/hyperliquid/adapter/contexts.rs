use std::{collections::BTreeMap, io::Read as _};

use ahash::AHashMap;
use bytes::{Bytes, BytesMut};
use market_participants_model::decimal::Price;
use parking_lot::Mutex;
use smol_str::SmolStr;

use super::implementation::{RouteListings, parse_price, protocol_error};
use crate::{
    error::MarketParticipantError, market_participants::model::NativeHyperliquidAssetContext,
};

const MAX_FAST_ASSET_CONTEXT_PAYLOAD: usize = 16 * 1024 * 1024;

const BOARD_COINS: usize = 2_048;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FieldUpdate<T> {
    #[default]
    Unchanged,
    Cleared,
    Set(T),
}

impl<T> FieldUpdate<T> {
    fn apply_to(self, held: &mut Option<T>) {
        match self {
            Self::Unchanged => {}
            Self::Cleared => *held = None,
            Self::Set(value) => *held = Some(value),
        }
    }
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for FieldUpdate<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Set(value),
            None => Self::Cleared,
        })
    }
}

#[derive(Clone, serde::Deserialize)]
struct FastContextEnvelope {
    channel: SmolStr,
    data: SmolStr,
}

#[derive(Clone, serde::Deserialize)]
struct FastContextValues {
    #[serde(default, rename = "markPx")]
    mark_px: FieldUpdate<SmolStr>,
    #[serde(default, rename = "midPx")]
    mid_px: FieldUpdate<SmolStr>,
}

struct ContextUpdate {
    coin: SmolStr,
    mark_px: FieldUpdate<Price>,
    mid_px: FieldUpdate<Price>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ContextEntry {
    mark_px: Option<Price>,
    mid_px: Option<Price>,
}

pub(super) struct FastContextBoard {
    coins: Mutex<AHashMap<SmolStr, ContextEntry>>,
}

impl Default for FastContextBoard {
    fn default() -> Self {
        Self {
            coins: Mutex::new(AHashMap::with_capacity(BOARD_COINS)),
        }
    }
}

impl FastContextBoard {
    pub(super) fn absorb(
        &self,
        payload: &[u8],
        listings: &RouteListings,
    ) -> Result<Option<Vec<NativeHyperliquidAssetContext>>, MarketParticipantError> {
        let Some(values) = decode_message(payload)? else {
            return Ok(None);
        };
        let mut updates = Vec::with_capacity(values.len());
        for (coin, value) in values {
            let watched = listings.contains_key(&coin);
            match read_update(coin, value) {
                Ok(update) => updates.push(update),
                Err(error) if !watched => {
                    log::warn!("a Hyperliquid fast asset context price was not read: {error}");
                }
                Err(error) => return Err(error),
            }
        }
        let mut coins = self.coins.lock();
        for update in updates {
            let entry = coins.entry(update.coin).or_default();
            update.mark_px.apply_to(&mut entry.mark_px);
            update.mid_px.apply_to(&mut entry.mid_px);
        }
        Ok(Some(board_contexts(&coins, listings)))
    }
}

fn read_update(
    coin: SmolStr,
    value: FastContextValues,
) -> Result<ContextUpdate, MarketParticipantError> {
    let mark_px = parse_update(value.mark_px, "decode a Hyperliquid fast asset mark price")?;
    let mid_px = parse_update(value.mid_px, "decode a Hyperliquid fast asset mid price")?;
    Ok(ContextUpdate {
        coin,
        mark_px,
        mid_px,
    })
}

fn board_contexts(
    coins: &AHashMap<SmolStr, ContextEntry>,
    listings: &RouteListings,
) -> Vec<NativeHyperliquidAssetContext> {
    listings
        .iter()
        .filter_map(|(coin, listing)| {
            let entry = coins.get(coin)?;
            if entry.mark_px.is_none() && entry.mid_px.is_none() {
                return None;
            }
            Some(NativeHyperliquidAssetContext {
                listing: listing.clone(),
                mark_price: entry.mark_px,
                mid_price: entry.mid_px,
            })
        })
        .collect()
}

fn parse_update(
    value: FieldUpdate<SmolStr>,
    operation: &'static str,
) -> Result<FieldUpdate<Price>, MarketParticipantError> {
    Ok(match value {
        FieldUpdate::Unchanged => FieldUpdate::Unchanged,
        FieldUpdate::Cleared => FieldUpdate::Cleared,
        FieldUpdate::Set(text) => FieldUpdate::Set(parse_price(&text, operation)?),
    })
}

fn decode_message(
    payload: &[u8],
) -> Result<Option<BTreeMap<SmolStr, FastContextValues>>, MarketParticipantError> {
    let Ok(envelope) = serde_json::from_slice::<FastContextEnvelope>(payload) else {
        return Ok(None);
    };
    if envelope.channel != "fastAssetCtxs" {
        return Ok(None);
    }
    let encoded = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        envelope.data.as_bytes(),
    )
    .map_err(|_| context_decode_failure(payload))?;
    let decoded = inflate(&encoded, payload)?;
    serde_json::from_slice(&decoded)
        .map(Some)
        .map_err(|_| context_decode_failure(payload))
}

fn context_decode_failure(payload: &[u8]) -> MarketParticipantError {
    MarketParticipantError::DecodeFailed {
        operation: SmolStr::new_static("decode Hyperliquid fast asset contexts"),
        raw_data: Bytes::copy_from_slice(payload),
    }
}

fn inflate(encoded: &[u8], payload: &[u8]) -> Result<Vec<u8>, MarketParticipantError> {
    let mut decoder = flate2::read::DeflateDecoder::new(encoded);
    let mut decoded = BytesMut::with_capacity(encoded.len().min(MAX_FAST_ASSET_CONTEXT_PAYLOAD));
    let mut limited = (&mut decoder).take((MAX_FAST_ASSET_CONTEXT_PAYLOAD + 1) as u64);
    let mut chunk = [0_u8; 8192];
    loop {
        let count = limited
            .read(&mut chunk)
            .map_err(|_| context_decode_failure(payload))?;
        if count == 0 {
            break;
        }
        decoded.extend_from_slice(&chunk[..count]);
    }
    if decoded.len() > MAX_FAST_ASSET_CONTEXT_PAYLOAD {
        return Err(protocol_error(
            "decode Hyperliquid fast asset contexts",
            SmolStr::new_static("the decompressed payload exceeds the limit"),
        ));
    }
    Ok(decoded.to_vec())
}

#[cfg(test)]
pub(super) fn fast_contexts_payload(contexts: &str) -> Vec<u8> {
    use std::io::Write as _;

    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(contexts.as_bytes())
        .expect("the fixture contexts should compress");
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        encoder
            .finish()
            .expect("the fixture compression should finish"),
    );
    serde_json::to_vec(&serde_json::json!({
        "channel": "fastAssetCtxs",
        "data": encoded,
    }))
    .expect("the fixture envelope should serialize")
}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, sync::Arc};

    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::{VenueListingKey, VenueRoute},
    };

    use super::{super::implementation::CORE_PERP_SCOPE, *};
    use crate::adapter::hyperliquid::catalog;

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Hyperliquid,
            VenueScopeId::new(scope).expect("the fixture scope is canonical"),
        )
    }

    fn perp_key(route: &VenueRoute, index_in_meta: u32) -> VenueListingKey {
        let asset_id =
            catalog::perp_asset_id(0, index_in_meta).expect("the fixture asset ID should fit");
        catalog::listing_key(route, catalog::perpetual_continuity(&route.scope, asset_id))
            .expect("the fixture listing key should derive")
    }

    fn listings(entries: &[(&str, u32)]) -> RouteListings {
        let route = route(CORE_PERP_SCOPE);
        Arc::new(
            entries
                .iter()
                .map(|(coin, index)| (SmolStr::new(*coin), perp_key(&route, *index)))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn prices(
        contexts: &[NativeHyperliquidAssetContext],
        index_in_meta: u32,
    ) -> Option<(Option<String>, Option<String>)> {
        let wanted = perp_key(&route(CORE_PERP_SCOPE), index_in_meta);
        contexts
            .iter()
            .find(|context| context.listing == wanted)
            .map(|context| {
                (
                    context.mark_price.as_ref().map(|price| price.to_string()),
                    context.mid_price.as_ref().map(|price| price.to_string()),
                )
            })
    }

    #[test]
    fn a_message_that_names_one_coin_does_not_drop_the_others() {
        let board = FastContextBoard::default();
        let listings = listings(&[("BTC", 0), ("ETH", 1), ("xyz:NVDA", 2)]);
        let opening = board
            .absorb(
                &fast_contexts_payload(
                    r#"{"BTC":{"markPx":"100.5","midPx":"100.25"},"ETH":{"markPx":"20","midPx":"20.1"}}"#,
                ),
                &listings,
            )
            .expect("the opening message decodes")
            .expect("the opening message is a context message");
        assert_eq!(
            (opening.len(), prices(&opening, 0), prices(&opening, 1)),
            (
                2,
                Some((Some("100.5".to_owned()), Some("100.25".to_owned()))),
                Some((Some("20".to_owned()), Some("20.1".to_owned()))),
            ),
            "the opening message is the whole board"
        );

        let delta = board
            .absorb(
                &fast_contexts_payload(r#"{"BTC":{"midPx":"100.75"}}"#),
                &listings,
            )
            .expect("the delta decodes")
            .expect("the delta is a context message");
        assert_eq!(
            prices(&delta, 1),
            Some((Some("20".to_owned()), Some("20.1".to_owned()))),
            "a coin the delta does not name keeps the price the venue published for it"
        );
        assert_eq!(
            prices(&delta, 0),
            Some((Some("100.5".to_owned()), Some("100.75".to_owned()))),
            "the field the delta names moves and the field it leaves out stands"
        );
    }

    #[test]
    fn a_field_the_message_leaves_out_is_not_a_cleared_price() {
        let board = FastContextBoard::default();
        let listings = listings(&[("BTC", 0)]);
        board
            .absorb(
                &fast_contexts_payload(r#"{"BTC":{"markPx":"100.5","midPx":"100.25"}}"#),
                &listings,
            )
            .expect("the opening message decodes");

        let mid_only = board
            .absorb(
                &fast_contexts_payload(r#"{"BTC":{"midPx":"100.75"}}"#),
                &listings,
            )
            .expect("the delta decodes")
            .expect("the delta is a context message");
        assert_eq!(
            prices(&mid_only, 0),
            Some((Some("100.5".to_owned()), Some("100.75".to_owned()))),
            "a delta that carries only the mid price leaves the mark price standing"
        );

        let cleared = board
            .absorb(
                &fast_contexts_payload(r#"{"BTC":{"markPx":null}}"#),
                &listings,
            )
            .expect("the delta decodes")
            .expect("the delta is a context message");
        assert_eq!(
            prices(&cleared, 0),
            Some((None, Some("100.75".to_owned()))),
            "a null mark price is the venue saying it has none"
        );

        let emptied = board
            .absorb(
                &fast_contexts_payload(r#"{"BTC":{"markPx":null,"midPx":null}}"#),
                &listings,
            )
            .expect("the delta decodes")
            .expect("the delta is a context message");
        assert!(
            prices(&emptied, 0).is_none(),
            "a coin with no price at all publishes no context"
        );
    }

    #[test]
    fn a_reader_that_never_saw_the_opening_message_still_reads_the_board() {
        let board = FastContextBoard::default();
        let first = listings(&[("BTC", 0)]);
        board
            .absorb(
                &fast_contexts_payload(r#"{"BTC":{"markPx":"100.5","midPx":"100.25"}}"#),
                &first,
            )
            .expect("the opening message decodes");

        let later = listings(&[("ETH", 1), ("BTC", 0)]);
        let answer = board
            .absorb(&fast_contexts_payload(r#"{"ETH":{"markPx":"20"}}"#), &later)
            .expect("the delta decodes")
            .expect("the delta is a context message");
        assert_eq!(
            prices(&answer, 0),
            Some((Some("100.5".to_owned()), Some("100.25".to_owned()))),
            "a reader that joined later reads the price the first reader's message carried"
        );
        assert_eq!(
            prices(&answer, 1),
            Some((Some("20".to_owned()), None)),
            "and the price its own message carried"
        );
    }

    #[test]
    fn a_coin_this_reader_does_not_list_publishes_no_context() {
        let board = FastContextBoard::default();
        let listings = listings(&[("BTC", 0)]);

        let contexts = board
            .absorb(
                &fast_contexts_payload(
                    r#"{"BTC":{"markPx":"100.5"},"xyz:NVDA":{"markPx":"465.13"}}"#,
                ),
                &listings,
            )
            .expect("the message decodes")
            .expect("the message is a context message");

        assert_eq!(
            contexts
                .iter()
                .map(|context| context.listing.route.scope.as_str())
                .collect::<Vec<_>>(),
            [CORE_PERP_SCOPE],
            "the board is read as the listings of the routes this reader monitors"
        );
        assert_eq!(
            contexts[0]
                .mark_price
                .as_ref()
                .map(|price| price.to_string()),
            Some("100.5".to_owned())
        );
    }

    #[test]
    fn a_message_of_another_channel_is_not_a_context_of_this_stream() {
        let board = FastContextBoard::default();
        let listings = listings(&[("BTC", 0)]);

        for payload in [
            br#"{"channel":"trades","data":[]}"#.as_slice(),
            br#"{"channel":"fastAssetCtxs"}"#.as_slice(),
            b"not json at all".as_slice(),
        ] {
            assert!(
                board
                    .absorb(payload, &listings)
                    .expect("a frame of another channel is not this stream's failure")
                    .is_none(),
                "only a context message is a context"
            );
        }
    }

    #[test]
    fn a_context_message_that_carries_something_else_is_refused() {
        let board = FastContextBoard::default();
        let listings = listings(&[("BTC", 0)]);

        let bad_base64 = serde_json::to_vec(&serde_json::json!({
            "channel": "fastAssetCtxs",
            "data": "not base64 at all",
        }))
        .expect("the fixture envelope should serialize");
        assert!(matches!(
            board.absorb(&bad_base64, &listings),
            Err(MarketParticipantError::DecodeFailed { operation, .. })
                if operation == "decode Hyperliquid fast asset contexts"
        ));

        let bad_body = serde_json::to_vec(&serde_json::json!({
            "channel": "fastAssetCtxs",
            "data": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                b"{\"BTC\": 7}",
            ),
        }))
        .expect("the fixture envelope should serialize");
        assert!(matches!(
            board.absorb(&bad_body, &listings),
            Err(MarketParticipantError::DecodeFailed { .. })
        ));
    }

    #[test]
    fn a_price_the_venue_spelled_badly_leaves_the_board_alone() {
        let board = FastContextBoard::default();
        let listings = listings(&[("BTC", 0), ("ETH", 1)]);
        board
            .absorb(
                &fast_contexts_payload(r#"{"BTC":{"markPx":"100.5"},"ETH":{"markPx":"20"}}"#),
                &listings,
            )
            .expect("the opening message decodes");

        let refused = board.absorb(
            &fast_contexts_payload(r#"{"BTC":{"markPx":"not a price"},"ETH":{"markPx":"21"}}"#),
            &listings,
        );
        assert!(matches!(
            refused,
            Err(MarketParticipantError::ProtocolError { operation, .. })
                if operation == "decode a Hyperliquid fast asset mark price"
        ));

        let answer = board
            .absorb(&fast_contexts_payload(r#"{"BTC":{}}"#), &listings)
            .expect("the next message decodes")
            .expect("the next message is a context message");
        assert_eq!(
            (prices(&answer, 0), prices(&answer, 1)),
            (
                Some((Some("100.5".to_owned()), None)),
                Some((Some("20".to_owned()), None)),
            ),
            "neither the malformed coin nor the coin beside it was merged"
        );
    }

    #[test]
    fn a_price_of_a_coin_this_reader_does_not_list_is_not_its_failure() {
        let board = FastContextBoard::default();
        let watched = listings(&[("BTC", 0)]);
        let watching_both = listings(&[("BTC", 0), ("xyz:NVDA", 2)]);
        board
            .absorb(
                &fast_contexts_payload(r#"{"BTC":{"markPx":"100.5"}}"#),
                &watched,
            )
            .expect("the opening message decodes");

        let answer = board
            .absorb(
                &fast_contexts_payload(
                    r#"{"BTC":{"markPx":"100.75"},"xyz:NVDA":{"markPx":"not a price"}}"#,
                ),
                &watched,
            )
            .expect("a market this reader does not list is not its failure")
            .expect("the message is a context message");
        assert_eq!(
            prices(&answer, 0),
            Some((Some("100.75".to_owned()), None)),
            "the price of the coin this reader does watch is still read"
        );

        assert!(matches!(
            board.absorb(
                &fast_contexts_payload(r#"{"xyz:NVDA":{"markPx":"not a price"}}"#),
                &watching_both,
            ),
            Err(MarketParticipantError::ProtocolError { operation, .. })
                if operation == "decode a Hyperliquid fast asset mark price"
        ));
    }

    #[test]
    fn a_payload_that_decompresses_past_the_limit_is_refused() {
        let board = FastContextBoard::default();
        let listings = listings(&[("BTC", 0)]);
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder
            .write_all(&vec![b'0'; MAX_FAST_ASSET_CONTEXT_PAYLOAD + 1])
            .expect("the fixture payload should compress");
        let payload = serde_json::to_vec(&serde_json::json!({
            "channel": "fastAssetCtxs",
            "data": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                encoder
                    .finish()
                    .expect("the fixture compression should finish"),
            ),
        }))
        .expect("the fixture envelope should serialize");

        assert!(matches!(
            board.absorb(&payload, &listings),
            Err(MarketParticipantError::ProtocolError { reason, .. })
                if reason.contains("exceeds the limit")
        ));
    }
}
