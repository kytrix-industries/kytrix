//! The venue's own board of live asset contexts, kept once for every reader of it.
//!
//! `fastAssetCtxs` is the venue's whole-venue context subscription: its body names no
//! scope and no asset, and what it publishes is not one message per price but a stream
//! of deltas over the venue's whole board of mark and mid prices. The first message
//! after the subscription opens is the board; every later one carries only the coins —
//! and, within a coin, only the fields — that moved since. A reader that took each
//! message for the whole board would drop every price that message did not happen to
//! name, which is most of them: a coin's own price is named in a small share of the
//! messages, so a wallet valued from the board would lose that coin's value on the
//! message after its price arrived and find it again on the next message that named it.
//!
//! The board is therefore kept where the subscription is — one socket of one venue
//! publishes one board — and every reader is handed the whole of it. That is also what
//! makes a second reader correct: the subscription is shared by topic, so a reader that
//! joins after the opening message never sees the opening message, and the board is what
//! makes its first answer as complete as the first reader's.

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

/// The largest fast asset context payload this adapter decompresses.
const MAX_FAST_ASSET_CONTEXT_PAYLOAD: usize = 16 * 1024 * 1024;

/// The coins one board is expected to hold, which is every asset the venue lists.
const BOARD_COINS: usize = 2_048;

/// One field of one coin's context update.
///
/// The venue distinguishes three things about a field and a plain `Option` collapses two
/// of them: a field the message does not carry has not moved, and a field the message
/// carries as null is the venue saying it has no such price any more. Only the second
/// may clear a price the board holds, so the wire shape says which one it is. (The live
/// stream has carried nulls only in its opening board, so the clearing case is modelled
/// from the shape rather than observed.)
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FieldUpdate<T> {
    /// The venue did not name this field: what the board holds for it stands.
    #[default]
    Unchanged,
    /// The venue named this field and published no value for it.
    Cleared,
    /// The venue published this value.
    Set(T),
}

impl<T> FieldUpdate<T> {
    /// Writes this update over the value the board holds for one field.
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
        // An absent field never reaches this: `#[serde(default)]` is what says a field a
        // message left out is unchanged.
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

/// The fields one coin's context carries in one message.
#[derive(Clone, serde::Deserialize)]
struct FastContextValues {
    #[serde(default, rename = "markPx")]
    mark_px: FieldUpdate<SmolStr>,
    #[serde(default, rename = "midPx")]
    mid_px: FieldUpdate<SmolStr>,
}

/// One coin's share of one message, with the prices it carried already read.
struct ContextUpdate {
    coin: SmolStr,
    mark_px: FieldUpdate<Price>,
    mid_px: FieldUpdate<Price>,
}

/// The prices the venue currently publishes for one coin.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ContextEntry {
    mark_px: Option<Price>,
    mid_px: Option<Price>,
}

/// The venue's live contexts, as the one subscription that carries them publishes them.
pub(super) struct FastContextBoard {
    /// The last price the venue published for each field of each coin it has named.
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
    /// Merges one socket message and answers the whole board of `listings`.
    ///
    /// The answer is the board rather than the message because that is what the venue's
    /// stream means: every price it has published, not the handful it just repeated.
    /// `None` is a frame of another channel, which this board has nothing to say about.
    pub(super) fn absorb(
        &self,
        payload: &[u8],
        listings: &RouteListings,
    ) -> Result<Option<Vec<NativeHyperliquidAssetContext>>, MarketParticipantError> {
        let Some(values) = decode_message(payload)? else {
            return Ok(None);
        };
        // Read every price this reader's own listings name before any of it is merged, so
        // one malformed price leaves the board as it was rather than half-updated.
        //
        // A price of a coin no listing here names is not this reader's to read: the venue
        // publishes the whole venue's board while this reader watches its own listings, and
        // one unreadable market must not fail a wallet pane that never reads it. Such a
        // coin is left out of the merge — it keeps the price the venue last published
        // readably — and the message is reported rather than dropped in silence.
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

/// Reads one coin's share of a message, or says why its price could not be read.
///
/// The mark price is read before the mid price so that the failure names the field the
/// venue spelled first.
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

/// The whole board, read as the contexts of the listings one reader asked about.
///
/// A coin the venue holds no price for publishes no context at all: a reader values what
/// a price was published for, and an empty entry would answer for the listing it names
/// instead of letting another market of the same asset answer for it.
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

/// Reads one wire field as the price it carries, keeping what it says about the field.
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

/// Decodes one socket frame into the updates it carries.
///
/// Every frame of the connection reaches every one of its streams, so a frame of another
/// channel, or one no channel of this venue wrote, is not a failure: it is a message this
/// stream has nothing to say about.
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

/// The failure of a context message that carried something else.
fn context_decode_failure(payload: &[u8]) -> MarketParticipantError {
    MarketParticipantError::DecodeFailed {
        operation: SmolStr::new_static("decode Hyperliquid fast asset contexts"),
        raw_data: Bytes::copy_from_slice(payload),
    }
}

/// Decompresses one deflate payload within this adapter's own limit.
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

/// The frame of one `fastAssetCtxs` message.
///
/// The tests that drive a real stream build their payloads with this, and the ones that
/// read the board directly use it too, so the shape under test is the shape the socket
/// delivers.
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

    /// The listing key of one asset of one perpetual route.
    fn perp_key(route: &VenueRoute, index_in_meta: u32) -> VenueListingKey {
        let asset_id =
            catalog::perp_asset_id(0, index_in_meta).expect("the fixture asset ID should fit");
        catalog::listing_key(route, catalog::perpetual_continuity(&route.scope, asset_id))
            .expect("the fixture listing key should derive")
    }

    /// The listings one reader decodes the shared stream against, by fixture asset index.
    fn listings(entries: &[(&str, u32)]) -> RouteListings {
        let route = route(CORE_PERP_SCOPE);
        Arc::new(
            entries
                .iter()
                .map(|(coin, index)| (SmolStr::new(*coin), perp_key(&route, *index)))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    /// The prices one answer carries for the fixture listing of one asset index.
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

    /// One message is a delta over the board, not the board.
    ///
    /// The venue names only the coins that moved, so a reader that took the message for
    /// the whole board would lose the value of every coin it did not name — which is the
    /// failure this board exists to prevent.
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

    /// A field the venue left out is not a price the venue cleared.
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

    /// Every reader of the one subscription is handed the board, not its own history.
    ///
    /// The venue sends the whole board once, when the subscription opens, and the topic is
    /// shared: a reader that joins later never sees that message. It still has to answer
    /// with the prices the venue published before it arrived.
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

    /// A coin no monitored route lists is not a context of this reader's board.
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

    /// A message of another channel is not a context message and not a failure.
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

    /// A payload that is not the message it claims to be is refused.
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

    /// One price the venue spelled badly leaves the board as it was.
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

    /// A price of a coin this reader does not list is not this reader's failure.
    ///
    /// The venue publishes the whole venue's board while a reader watches its own listings,
    /// and one board serves every reader of the subscription: a market nothing here reads
    /// must not fail a wallet pane that never reads it — and its unreadable price must not
    /// be merged either.
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

        // The same message read by a reader whose listings do name that coin is refused: a
        // market the reader reads and cannot price is that reader's failure.
        assert!(matches!(
            board.absorb(
                &fast_contexts_payload(r#"{"xyz:NVDA":{"markPx":"not a price"}}"#),
                &watching_both,
            ),
            Err(MarketParticipantError::ProtocolError { operation, .. })
                if operation == "decode a Hyperliquid fast asset mark price"
        ));
    }

    /// A message that decompresses past the adapter's own limit is refused.
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
