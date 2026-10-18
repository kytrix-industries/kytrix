//! OKX market-data operation helpers and REST response decoders.
//!
//! Every decoder answers with the venue's own numbers paired with the listing
//! the venue named: the public market-data endpoints are addressed by `instId`,
//! so the identity of a row is resolved here, at the last place where the
//! venue's own identifier is authoritative.

use bytes::Bytes;
use market_participants_model::{
    decimal::{Price, Quantity},
    identifier::NativeSymbol,
    listing::{
        route::{VenueListingKey, VenueRoute},
        venue::OkxInstrumentType,
    },
    market::{
        book::MarketSequence,
        units::{MarketTradeId, Side},
    },
};
use serde::{Deserialize, de::DeserializeOwned};
use smol_str::{SmolStr, format_smolstr};

use super::CODEC;
use crate::{
    adapter::{
        ExecutionContext, NativeCandlestick, NativeListing, NativeOrderBook, NativeTicker,
        NativeTrade,
    },
    error::{MarketParticipantError, NetworkError},
    market_participants::model::NativeOrderBookLevel,
};

/// One public OKX REST GET against the endpoint this request was resolved for.
pub(crate) async fn public_get(
    context: &ExecutionContext,
    path: &str,
    query: &[(&str, String)],
) -> Result<Bytes, MarketParticipantError> {
    let url = super::adapter::http_url(context, path, query)?;
    let response = context
        .http_client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| MarketParticipantError::ConnectionFailed {
            route: context.route(),
            endpoint: format_smolstr!("{}", url.origin().ascii_serialization()),
            source: NetworkError::Io {
                message: format_smolstr!("{error}"),
            },
        })?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| MarketParticipantError::DecodeFailed {
            operation: "read OKX market-data response".into(),
            raw_data: Bytes::from(error.to_string()),
        })?;
    if !status.is_success() {
        return Err(CODEC.response_error(
            &context.route(),
            url.origin().ascii_serialization().as_str(),
            status,
            body,
        ));
    }
    Ok(body)
}

/// Decodes one `{ code, msg, data }` OKX response and returns its rows.
pub(crate) fn decode_public<T: DeserializeOwned>(
    body: &[u8],
    operation: &'static str,
) -> Result<Vec<T>, MarketParticipantError> {
    let response: super::OkxResponse<T> =
        serde_json::from_slice(body).map_err(|_| MarketParticipantError::DecodeFailed {
            operation: operation.into(),
            raw_data: Bytes::copy_from_slice(body),
        })?;
    if response.code != "0" {
        return Err(CODEC.map_error(
            response.code.as_str(),
            response.msg.unwrap_or_default(),
            Some(Bytes::copy_from_slice(body)),
        ));
    }
    Ok(response.data)
}

/// The position of the traded volume in one OKX candlestick row.
///
/// OKX counts the volume of a candle in the unit the instrument's own quantity
/// rule names, and publishes it at this one position for every product line: a
/// spot row counts the base currency, a derivative row counts contracts, and the
/// base amount of a derivative follows in the next field. The model labels this
/// field with the listing's own `QuantityUnit`, so reading the base amount of a
/// derivative here would publish a base amount under the `Contracts` unit its
/// listing sizes orders in; the `candle` channel pushes the same position and is
/// therefore decoded into the same field-to-unit mapping.
const CANDLE_VOLUME_FIELD: usize = 5;

/// Decodes one candlestick answer.
pub(crate) fn decode_candlesticks(
    body: &[u8],
) -> Result<Vec<NativeCandlestick>, MarketParticipantError> {
    let rows: Vec<OkxCandleRow> = decode_public(body, "decode OKX candlestick HTTP response")?;
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        values.push(NativeCandlestick {
            timestamp: parse_i64(
                row.field(0, "decode OKX candlestick timestamp", body)?,
                body,
                "decode OKX candlestick timestamp",
            )?,
            open: parse_price(
                row.field(1, "decode OKX candlestick open", body)?,
                body,
                "decode OKX candlestick open",
            )?,
            high: parse_price(
                row.field(2, "decode OKX candlestick high", body)?,
                body,
                "decode OKX candlestick high",
            )?,
            low: parse_price(
                row.field(3, "decode OKX candlestick low", body)?,
                body,
                "decode OKX candlestick low",
            )?,
            close: parse_price(
                row.field(4, "decode OKX candlestick close", body)?,
                body,
                "decode OKX candlestick close",
            )?,
            volume: parse_quantity(
                row.field(CANDLE_VOLUME_FIELD, "decode OKX candlestick volume", body)?,
                body,
                "decode OKX candlestick volume",
            )?,
        });
    }
    values.sort_unstable_by_key(|value| value.timestamp);
    Ok(values)
}

/// Decodes the one ticker of one instrument.
pub(crate) fn decode_ticker(
    instrument_type: OkxInstrumentType,
    listing: &VenueListingKey,
    instrument_id: &str,
    body: &[u8],
) -> Result<NativeListing<NativeTicker>, MarketParticipantError> {
    let [item] = decode_public::<OkxTickerData>(body, "decode OKX ticker HTTP response")?
        .try_into()
        .map_err(|_| MarketParticipantError::ProtocolError {
            operation: "decode OKX ticker HTTP response".into(),
            reason: "ticker response must contain exactly one item".into(),
            raw_data: Some(Bytes::copy_from_slice(body)),
        })?;
    if item.inst_id.as_str() != instrument_id {
        return Err(MarketParticipantError::ProtocolError {
            operation: "decode OKX ticker instrument".into(),
            reason: "ticker instrument does not match request".into(),
            raw_data: Some(Bytes::copy_from_slice(body)),
        });
    }
    Ok(NativeListing::new(
        listing.clone(),
        ticker_of(instrument_type, &item, body)?,
    ))
}

/// Decodes every ticker of one route, each under the listing the venue named.
///
/// A route answers for instruments its published catalog also holds: the listing
/// key is derived from the venue's own identifier, and a ticker whose instrument
/// the venue named cannot resolve is a protocol failure rather than a row
/// silently attributed to another listing.
pub(crate) fn decode_tickers(
    route: &VenueRoute,
    instrument_type: OkxInstrumentType,
    body: &[u8],
) -> Result<Vec<NativeListing<NativeTicker>>, MarketParticipantError> {
    let rows = decode_public::<OkxTickerData>(body, "decode OKX all-tickers HTTP response")?;
    let mut tickers = Vec::with_capacity(rows.len());
    for row in rows {
        let listing = CODEC.listing_key(route, instrument_type, row.inst_id.as_str())?;
        let ticker = ticker_of(instrument_type, &row, body)?;
        tickers.push(NativeListing::new(listing, ticker));
    }
    tickers.sort_unstable_by(|left, right| left.listing.cmp(&right.listing));
    Ok(tickers)
}

/// The ticker of one instrument, as OKX publishes it on the HTTP endpoint and on
/// the `tickers` channel.
pub(crate) fn ticker_of(
    instrument_type: OkxInstrumentType,
    item: &OkxTickerData,
    body: &[u8],
) -> Result<NativeTicker, MarketParticipantError> {
    let close = parse_price(&item.last, body, "decode OKX ticker close")?;
    let (traded_quantity, traded_notional) = decode_ticker_volumes(
        instrument_type,
        &item.vol_24h,
        &item.vol_ccy_24h,
        close,
        body,
    )?;
    Ok(NativeTicker {
        // OKX publishes no separate rolling-statistics counter, so the `ts` of
        // the snapshot is the venue's own marker for the statistics it carries.
        monotonic_id: Some(parse_i64(&item.ts, body, "decode OKX ticker timestamp")?),
        open: parse_price(&item.open_24h, body, "decode OKX ticker open")?,
        high: Some(parse_price(&item.high_24h, body, "decode OKX ticker high")?),
        low: Some(parse_price(&item.low_24h, body, "decode OKX ticker low")?),
        close,
        traded_quantity: Some(traded_quantity),
        traded_notional: Some(traded_notional),
    })
}

/// The 24-hour volumes of one ticker, as a traded amount and a monetary amount.
///
/// OKX switches the unit of both fields with the instrument type, and each field
/// is read for the unit the listing's own quantity rule names. A spot ticker
/// counts base currency in `vol24h` and quote currency in `volCcy24h`; a
/// derivative counts *contracts* in `vol24h` and base currency in `volCcy24h`.
/// The quote amount of a derivative is therefore its base amount priced at the
/// ticker's last price, while its traded quantity is the contract count — the
/// same unit OKX sizes a derivative order in.
///
/// The HTTP ticker and the `tickers` channel publish the two fields with the
/// same units, so both decoders read them through this function.
pub(crate) fn decode_ticker_volumes(
    instrument_type: OkxInstrumentType,
    vol_24h: &SmolStr,
    vol_ccy_24h: &SmolStr,
    close: Price,
    body: &[u8],
) -> Result<(Quantity, Quantity), MarketParticipantError> {
    match instrument_type {
        OkxInstrumentType::Spot => Ok((
            parse_quantity(vol_24h, body, "decode OKX ticker traded quantity")?,
            parse_quantity(vol_ccy_24h, body, "decode OKX ticker traded notional")?,
        )),
        OkxInstrumentType::Perpetual | OkxInstrumentType::Future | OkxInstrumentType::Option => {
            let traded_quantity =
                parse_quantity(vol_24h, body, "decode OKX ticker traded quantity")?;
            let traded_base =
                parse_quantity(vol_ccy_24h, body, "decode OKX ticker traded base amount")?;
            let traded_notional = close.checked_mul_quantity(traded_base).ok_or_else(|| {
                MarketParticipantError::ProtocolError {
                    operation: "decode OKX ticker traded notional".into(),
                    reason: "traded notional overflowed fixed-point range".into(),
                    raw_data: Some(Bytes::copy_from_slice(body)),
                }
            })?;
            Ok((traded_quantity, traded_notional))
        }
    }
}

/// Decodes one order-book snapshot of one listing.
pub(crate) fn decode_order_book(
    listing: &VenueListingKey,
    body: &[u8],
) -> Result<NativeListing<NativeOrderBook>, MarketParticipantError> {
    let [item] =
        decode_public::<OkxHttpOrderBookData>(body, "decode OKX order-book HTTP response")?
            .try_into()
            .map_err(|_| MarketParticipantError::ProtocolError {
                operation: "decode OKX order-book HTTP response".into(),
                reason: "order-book response must contain exactly one item".into(),
                raw_data: Some(Bytes::copy_from_slice(body)),
            })?;
    // OKX numbers its books; a snapshot that carries no `seqId` cannot be
    // replayed against a later update, so its absence is a protocol failure
    // rather than a zero this adapter would have invented.
    let sequence = item
        .seq_id
        .ok_or_else(|| MarketParticipantError::ProtocolError {
            operation: "decode OKX order-book sequence".into(),
            reason: "order-book response omitted seqId".into(),
            raw_data: Some(Bytes::copy_from_slice(body)),
        })?;
    let value = NativeOrderBook {
        sequence: MarketSequence::Numeric(sequence),
        timestamp: Some(parse_i64(
            &item.ts,
            body,
            "decode OKX order-book timestamp",
        )?),
        bids: decode_levels(item.bids, body, "decode OKX order-book bids")?,
        asks: decode_levels(item.asks, body, "decode OKX order-book asks")?,
    };
    Ok(NativeListing::new(
        listing.clone(),
        normalize_order_book(value)?,
    ))
}

/// Decodes the recent market trades of one listing.
pub(crate) fn decode_trades(
    listing: &VenueListingKey,
    instrument_id: &str,
    body: &[u8],
) -> Result<Vec<NativeListing<NativeTrade>>, MarketParticipantError> {
    let items: Vec<OkxHttpTradeData> =
        decode_public(body, "decode OKX market-trades HTTP response")?;
    let mut trades = Vec::with_capacity(items.len());
    for item in items {
        if item.inst_id.as_str() != instrument_id {
            return Err(MarketParticipantError::ProtocolError {
                operation: "decode OKX trade instrument".into(),
                reason: "trade instrument does not match request".into(),
                raw_data: Some(Bytes::copy_from_slice(body)),
            });
        }
        let side = match item.side.as_str() {
            "buy" => Side::Buy,
            "sell" => Side::Sell,
            _ => {
                return Err(MarketParticipantError::ProtocolError {
                    operation: "decode OKX trade side".into(),
                    reason: "trade side must be buy or sell".into(),
                    raw_data: Some(Bytes::copy_from_slice(body)),
                });
            }
        };
        let trade = NativeTrade {
            id: MarketTradeId::Numeric(parse_u64(&item.trade_id, body, "decode OKX trade ID")?),
            timestamp: parse_i64(&item.ts, body, "decode OKX trade timestamp")?,
            price: parse_price(&item.px, body, "decode OKX trade price")?,
            quantity: parse_quantity(&item.sz, body, "decode OKX trade quantity")?,
            side,
        };
        trades.push(NativeListing::new(listing.clone(), trade));
    }
    trades.sort_unstable_by(|left, right| {
        (left.data.timestamp, &left.data.id).cmp(&(right.data.timestamp, &right.data.id))
    });
    Ok(trades)
}

fn decode_levels(
    rows: Vec<OkxHttpLevelRow>,
    body: &[u8],
    operation: &'static str,
) -> Result<Vec<NativeOrderBookLevel>, MarketParticipantError> {
    rows.into_iter()
        .map(|row| {
            Ok(NativeOrderBookLevel {
                price: parse_price(row.field(0, operation, body)?, body, operation)?,
                quantity: parse_quantity(row.field(1, operation, body)?, body, operation)?,
            })
        })
        .collect()
}

/// Orders one book the way its own sides are published.
///
/// OKX pushes the removal of a level as a row of quantity zero, and its sides
/// arrive in the order the venue's matching engine holds them; the levels that
/// remain are sorted by price so the book is the one the model requires.
fn normalize_order_book(
    mut value: NativeOrderBook,
) -> Result<NativeOrderBook, MarketParticipantError> {
    value.bids.retain(|level| !level.quantity.is_zero());
    value.asks.retain(|level| !level.quantity.is_zero());
    value
        .bids
        .sort_unstable_by_key(|level| std::cmp::Reverse(level.price));
    value.asks.sort_unstable_by_key(|level| level.price);
    Ok(value)
}

fn parse_i64(
    value: &str,
    body: &[u8],
    operation: &'static str,
) -> Result<i64, MarketParticipantError> {
    value
        .parse::<i64>()
        .map_err(|error| MarketParticipantError::ProtocolError {
            operation: operation.into(),
            reason: format_smolstr!("{error}"),
            raw_data: Some(Bytes::copy_from_slice(body)),
        })
}

fn parse_u64(
    value: &str,
    body: &[u8],
    operation: &'static str,
) -> Result<u64, MarketParticipantError> {
    value
        .parse::<u64>()
        .map_err(|error| MarketParticipantError::ProtocolError {
            operation: operation.into(),
            reason: format_smolstr!("{error}"),
            raw_data: Some(Bytes::copy_from_slice(body)),
        })
}

fn parse_price(
    value: &str,
    body: &[u8],
    operation: &'static str,
) -> Result<Price, MarketParticipantError> {
    value
        .parse::<Price>()
        .map_err(|error| MarketParticipantError::ProtocolError {
            operation: operation.into(),
            reason: format_smolstr!("{error}"),
            raw_data: Some(Bytes::copy_from_slice(body)),
        })
}

fn parse_quantity(
    value: &str,
    body: &[u8],
    operation: &'static str,
) -> Result<Quantity, MarketParticipantError> {
    value
        .parse::<Quantity>()
        .map_err(|error| MarketParticipantError::ProtocolError {
            operation: operation.into(),
            reason: format_smolstr!("{error}"),
            raw_data: Some(Bytes::copy_from_slice(body)),
        })
}

/// One OKX candlestick row, which is a positional array of strings.
#[derive(Debug, Deserialize)]
struct OkxCandleRow(Vec<SmolStr>);

impl OkxCandleRow {
    fn field<'a>(
        &'a self,
        index: usize,
        operation: &'static str,
        body: &[u8],
    ) -> Result<&'a str, MarketParticipantError> {
        self.0
            .get(index)
            .map(SmolStr::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| MarketParticipantError::ProtocolError {
                operation: operation.into(),
                reason: format_smolstr!("candlestick row is missing field {index}"),
                raw_data: Some(Bytes::copy_from_slice(body)),
            })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct OkxTickerData {
    #[serde(rename = "instId")]
    inst_id: NativeSymbol,
    ts: SmolStr,
    #[serde(rename = "open24h")]
    open_24h: SmolStr,
    #[serde(rename = "high24h")]
    high_24h: SmolStr,
    #[serde(rename = "low24h")]
    low_24h: SmolStr,
    last: SmolStr,
    #[serde(rename = "vol24h")]
    vol_24h: SmolStr,
    #[serde(rename = "volCcy24h")]
    vol_ccy_24h: SmolStr,
}

impl OkxTickerData {
    /// Returns the venue instrument this ticker is about.
    pub(crate) fn inst_id(&self) -> &NativeSymbol {
        &self.inst_id
    }
}

#[derive(Debug, Deserialize)]
struct OkxHttpOrderBookData {
    #[serde(rename = "seqId")]
    seq_id: Option<u64>,
    ts: SmolStr,
    bids: Vec<OkxHttpLevelRow>,
    asks: Vec<OkxHttpLevelRow>,
}

#[derive(Debug, Deserialize)]
struct OkxHttpLevelRow(Vec<SmolStr>);

impl OkxHttpLevelRow {
    fn field<'a>(
        &'a self,
        index: usize,
        operation: &'static str,
        body: &[u8],
    ) -> Result<&'a str, MarketParticipantError> {
        self.0
            .get(index)
            .map(SmolStr::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| MarketParticipantError::ProtocolError {
                operation: operation.into(),
                reason: format_smolstr!("order-book level is missing field {index}"),
                raw_data: Some(Bytes::copy_from_slice(body)),
            })
    }
}

#[derive(Debug, Deserialize)]
struct OkxHttpTradeData {
    #[serde(rename = "instId")]
    inst_id: NativeSymbol,
    #[serde(rename = "tradeId")]
    trade_id: SmolStr,
    ts: SmolStr,
    px: SmolStr,
    sz: SmolStr,
    side: SmolStr,
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::{route::VenueRoute, venue::OkxInstrumentType},
        market::{book::MarketSequence, units::Side},
    };

    use super::{
        decode_candlesticks, decode_order_book, decode_ticker, decode_tickers, decode_trades,
    };
    use crate::routing::{SPOT_SCOPE, SWAP_SCOPE};

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Okx,
            VenueScopeId::new(scope).expect("the scope id is canonical"),
        )
    }

    fn listing(
        scope: &str,
        instrument_type: OkxInstrumentType,
        instrument_id: &str,
    ) -> (
        VenueRoute,
        market_participants_model::listing::route::VenueListingKey,
    ) {
        let route = route(scope);
        let key = super::CODEC
            .listing_key(&route, instrument_type, instrument_id)
            .expect("the fixture instrument id is a stable identity");
        (route, key)
    }

    #[test]
    fn decodes_public_market_data_fixtures() {
        let candles = decode_candlesticks(&Bytes::from_static(
            br#"{"code":"0","msg":"","data":[["1700000000000","100","110","90","105","12","12","1260","1"]]}"#,
        ))
        .expect("candles decode");
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].close.to_string(), "105");
        assert_eq!(candles[0].volume.to_string(), "12");

        let (_, spot_listing) = listing(SPOT_SCOPE, OkxInstrumentType::Spot, "BTC-USDT");
        let ticker = decode_ticker(
            OkxInstrumentType::Spot,
            &spot_listing,
            "BTC-USDT",
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","ts":"1700000000000","open24h":"100","high24h":"110","low24h":"90","last":"105","vol24h":"12","volCcy24h":"1260"}]}"#,
            ),
        )
        .expect("ticker decode");
        assert_eq!(ticker.listing, spot_listing);
        assert_eq!(ticker.data.close.to_string(), "105");
        assert_eq!(
            ticker
                .data
                .traded_quantity
                .map(|value| value.to_string())
                .as_deref(),
            Some("12")
        );
        assert_eq!(
            ticker
                .data
                .traded_notional
                .map(|value| value.to_string())
                .as_deref(),
            Some("1260")
        );

        let tickers = decode_tickers(
            &route(SPOT_SCOPE),
            OkxInstrumentType::Spot,
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","ts":"1700000000000","open24h":"100","high24h":"110","low24h":"90","last":"105","vol24h":"12","volCcy24h":"1260"},{"instId":"ETH-USDT","ts":"1700000000001","open24h":"200","high24h":"210","low24h":"190","last":"205","vol24h":"4","volCcy24h":"820"}]}"#,
            ),
        )
        .expect("all tickers decode");
        assert_eq!(tickers.len(), 2);
        assert_ne!(tickers[0].listing, tickers[1].listing);

        let order_book = decode_order_book(
            &spot_listing,
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"seqId":7,"ts":"1700000000000","bids":[["100","2","0","1"]],"asks":[["101","3","0","1"]]}]}"#,
            ),
        )
        .expect("order book decode");
        assert_eq!(order_book.data.sequence, MarketSequence::Numeric(7));
        assert_eq!(order_book.data.bids.len(), 1);

        let trades = decode_trades(
            &spot_listing,
            "BTC-USDT",
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","tradeId":"41","ts":"1700000000000","px":"101.25","sz":"2.5","side":"sell"}]}"#,
            ),
        )
        .expect("trades decode");
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].listing, spot_listing);
        assert_eq!(trades[0].data.side, Side::Sell);
        assert_eq!(trades[0].data.quantity.to_string(), "2.5");
    }

    /// OKX switches the unit of both 24-hour volumes with the instrument type:
    /// the derivative branch reads the contract count as the traded quantity —
    /// the unit its listing declares — and prices the base amount into the quote
    /// amount.
    #[test]
    fn ticker_volumes_follow_the_unit_of_the_instrument_type() {
        let (_, spot_listing) = listing(SPOT_SCOPE, OkxInstrumentType::Spot, "BTC-USDT");
        let spot_ticker = decode_ticker(
            OkxInstrumentType::Spot,
            &spot_listing,
            "BTC-USDT",
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","ts":"1700000000000","open24h":"100","high24h":"110","low24h":"90","last":"105","vol24h":"12","volCcy24h":"1260"}]}"#,
            ),
        )
        .expect("spot ticker decode");
        assert_eq!(
            spot_ticker.data.traded_quantity.map(|v| v.to_string()),
            Some("12".into())
        );
        assert_eq!(
            spot_ticker.data.traded_notional.map(|v| v.to_string()),
            Some("1260".into())
        );

        let (_, swap_listing) = listing(SWAP_SCOPE, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP");
        let perpetual_ticker = decode_ticker(
            OkxInstrumentType::Perpetual,
            &swap_listing,
            "BTC-USDT-SWAP",
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","ts":"1700000000000","open24h":"100","high24h":"110","low24h":"90","last":"105","vol24h":"1200","volCcy24h":"12"}]}"#,
            ),
        )
        .expect("perpetual ticker decode");
        assert_eq!(
            perpetual_ticker.data.traded_quantity.map(|v| v.to_string()),
            Some("1200".into()),
            "a derivative trades in the contracts its quantity rule names"
        );
        assert_eq!(
            perpetual_ticker.data.traded_notional.map(|v| v.to_string()),
            Some("1260".into()),
            "the base amount is priced into the quote amount"
        );
    }

    /// One real OKX swap answer: the two volume fields differ by the contract
    /// multiplier, so reading the wrong one reports the base amount as if it were
    /// the contract count.
    #[test]
    fn a_derivative_volume_is_read_as_contracts_not_as_base_currency() {
        let (_, swap_listing) = listing(SWAP_SCOPE, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP");
        let ticker = decode_ticker(
            OkxInstrumentType::Perpetual,
            &swap_listing,
            "BTC-USDT-SWAP",
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","ts":"1700000000000","open24h":"95000","high24h":"96000","low24h":"94000","last":"95532.4","vol24h":"9732157.46","volCcy24h":"97321.5746"}]}"#,
            ),
        )
        .expect("perpetual ticker decode");

        assert_eq!(
            ticker.data.traded_quantity.map(|v| v.to_string()),
            Some("9732157.46".into()),
            "vol24h is the contract count"
        );
        assert_eq!(
            ticker.data.traded_notional.map(|v| v.to_string()),
            Some("9297363593.31704".into()),
            "volCcy24h is the base amount, priced at the last price"
        );
    }

    /// A derivative candle counts contracts first and the base amount second, so
    /// reading the base amount would report a hundred times the traded quantity of
    /// a swap whose contract is worth a hundredth of its base currency. The value
    /// the model labels with the listing's unit is the contract count.
    #[test]
    fn derivative_candles_read_the_contract_count_not_the_base_amount() {
        let candles = decode_candlesticks(&Bytes::from_static(
            br#"{"code":"0","msg":"","data":[["1700000000000","95000","96000","94000","95532.4","9732157.46","97321.5746","9297363593.31704","1"]]}"#,
        ))
        .expect("candles decode");

        assert_eq!(candles[0].close.to_string(), "95532.4");
        assert_eq!(
            candles[0].volume.to_string(),
            "9732157.46",
            "field 5 is the contract count a derivative listing sizes orders in"
        );
        assert_ne!(
            candles[0].volume.to_string(),
            "97321.5746",
            "field 6 is the base amount, which is not the traded quantity"
        );
    }

    /// A route-level ticker answer names its instruments, and each row keeps the
    /// listing key its own instrument derives.
    #[test]
    fn route_level_tickers_keep_each_rows_own_listing() {
        let swap = route(SWAP_SCOPE);
        let tickers = decode_tickers(
            &swap,
            OkxInstrumentType::Perpetual,
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","ts":"1700000000000","open24h":"100","high24h":"110","low24h":"90","last":"105","vol24h":"1200","volCcy24h":"12"},{"instId":"ETH-USDT-SWAP","ts":"1700000000001","open24h":"200","high24h":"210","low24h":"190","last":"205","vol24h":"400","volCcy24h":"4"}]}"#,
            ),
        )
        .expect("all tickers decode");

        let btc = super::CODEC
            .listing_key(&swap, OkxInstrumentType::Perpetual, "BTC-USDT-SWAP")
            .expect("the instrument id is a stable identity");
        let eth = super::CODEC
            .listing_key(&swap, OkxInstrumentType::Perpetual, "ETH-USDT-SWAP")
            .expect("the instrument id is a stable identity");
        assert_eq!(tickers.len(), 2);
        assert_eq!(tickers[0].listing, btc);
        assert_eq!(tickers[1].listing, eth);
    }

    /// An order-book answer without a sequence cannot be replayed, so it is
    /// refused instead of being published under an invented sequence of zero.
    #[test]
    fn an_order_book_without_a_sequence_is_refused() {
        let (_, spot_listing) = listing(SPOT_SCOPE, OkxInstrumentType::Spot, "BTC-USDT");
        let error = decode_order_book(
            &spot_listing,
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"ts":"1700000000000","bids":[["100","2"]],"asks":[["101","3"]]}]}"#,
            ),
        )
        .expect_err("a book without a sequence must not be published");

        assert!(matches!(
            error,
            crate::error::MarketParticipantError::ProtocolError { operation, .. }
                if operation == "decode OKX order-book sequence"
        ));
    }

    /// OKX pushes the removal of a level as a zero-quantity row, and a book is
    /// only the model's book once its sides are ordered.
    #[test]
    fn order_book_levels_drop_zero_quantities_and_are_ordered() {
        let (_, spot_listing) = listing(SPOT_SCOPE, OkxInstrumentType::Spot, "BTC-USDT");
        let book = decode_order_book(
            &spot_listing,
            &Bytes::from_static(
                br#"{"code":"0","msg":"","data":[{"seqId":9,"ts":"1700000000000","bids":[["99","1"],["100","0"],["98","2"]],"asks":[["103","1"],["101","0"],["102","2"]]}]}"#,
            ),
        )
        .expect("order book decode");

        assert_eq!(
            book.data
                .bids
                .iter()
                .map(|level| level.price.to_string())
                .collect::<Vec<_>>(),
            vec!["99".to_owned(), "98".to_owned()]
        );
        assert_eq!(
            book.data
                .asks
                .iter()
                .map(|level| level.price.to_string())
                .collect::<Vec<_>>(),
            vec!["102".to_owned(), "103".to_owned()]
        );
    }

    #[test]
    fn rejects_malformed_market_data_without_panicking() {
        let (_, spot_listing) = listing(SPOT_SCOPE, OkxInstrumentType::Spot, "BTC-USDT");
        let malformed = Bytes::from_static(
            br#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","tradeId":"1","ts":"1700000000000","px":"1","sz":"1","side":"other"}]}"#,
        );
        assert!(decode_trades(&spot_listing, "BTC-USDT", &malformed).is_err());

        let malformed_candle =
            Bytes::from_static(br#"{"code":"0","msg":"","data":[["1700000000000","100"]]}"#);
        assert!(decode_candlesticks(&malformed_candle).is_err());
    }
}
