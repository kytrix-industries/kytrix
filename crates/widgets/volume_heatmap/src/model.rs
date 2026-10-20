//! What one venue's heat map is made of, and how one tile reads.
//!
//! The map answers one question — which instruments of one venue moved the most
//! money — so a tile is a **canonical instrument**, not a listing. The same
//! instrument trades on several scopes of one venue, and all of that is the same
//! money moving; a map that listed `BTC/USDT` once per scope would show one
//! instrument twice and neither reading whole.
//!
//! The map is drawn from **one venue's answers**, and this module never sees a
//! second venue's. That is not an accident of the folding below but the whole
//! point of it: two venues quoting the same instrument are two numbers about two
//! order books, and a map that added them would be reporting a market that does
//! not exist. The pane chooses the venue and the service asks only for it, so the
//! fold has nothing to keep apart.
//!
//! Two things follow from a tile being one instrument, and both are deliberate:
//!
//! - The size of a tile is the **sum** of every contributing listing's notional,
//!   but only across listings whose money is denominated in the *same* asset. A
//!   dollar read in USDT and a dollar read in USDC are not added together: the
//!   two are different units, and merging them would produce a number that means
//!   nothing. A tile therefore names the asset its money is denominated in.
//! - Every row was summed from at least one listing of the venue the map is drawn
//!   from. A row with no contributing listing does not exist.
//!
//! The tile still has to take *one* colour, so the largest individual source is
//! the one it speaks for: the listing that most of the summed money went through,
//! and whose price therefore is the move the tile is reporting.
//!
//! Everything here is pure: it takes what one venue answered and produces the
//! list of tiles, with the labels already formatted. Nothing in this module knows
//! about GPUI or about the streams the answers arrive on.

use std::{cmp::Ordering, sync::Arc};

use ahash::{AHashMap, HashMap, HashMapExt};
use market_participants_connector::catalog::registry::asset::AssetTerms;
use market_participants_model::{
    asset::{AssetId, TokenReference},
    decimal::{Price, Quantity},
    instrument::{Instrument, InstrumentKind, InstrumentTerms},
    listing::route::VenueListingKey,
    market::{
        selection::ListingTicker,
        units::{MarketQuantity, QuantityUnit},
    },
};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;

/// How many tiles the finished map shows.
///
/// It is also what the layout, the text cache and the pane's own body are sized
/// for, so it lives here rather than in each of them.
pub(super) const TILE_LIMIT: usize = 20;

/// The token code an accepted notional is denominated in when it is tether.
const USDT_CODE: &str = "USDT";

/// The token code an accepted notional is denominated in when it is USD Coin.
const USDC_CODE: &str = "USDC";

/// The assets that are themselves a dollar, and would therefore be the largest
/// tile on the map without being a market anyone is trading.
///
/// The set is read against the *canonical terms* of an asset — an ISO 4217 code
/// or a verified native token code — and never against a display code or a rendered
/// label. A rule that read a rendered string would change its answer the day the
/// catalog published a different code for the same asset.
const EXCLUDED_BASE_CODES: [&str; 6] = ["U", "USDC", "USD1", "USD", "FDUSD", "USDS"];

/// The 18 fractional digits a quantity carries, which is what makes the compact
/// formatter exact rather than a floating-point guess.
const FIXED_SCALE: u128 = 1_000_000_000_000_000_000;

/// The units a compact notional is written in, largest first.
const DISPLAY_UNITS: [(u128, &str); 6] = [
    (1_000_000_000_000_000, "Q"),
    (1_000_000_000_000, "T"),
    (1_000_000_000, "B"),
    (1_000_000, "M"),
    (1_000, "K"),
    (1, ""),
];

/// Which way the price of the listing a tile speaks for moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PriceDirection {
    Down,
    Flat,
    Up,
}

/// One listing a route publishes, with the canonical instrument it resolves to.
///
/// The instrument is what makes a listing's product family and its own assets
/// readable: nothing here reads a `base`, `quote` or `settle` field name, and a
/// listing of a family this map does not map is never turned into a tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteListing {
    pub listing: VenueListingKey,
    pub instrument: Instrument,
}

/// Everything one route answered, with the labels its assets are shown under.
///
/// The labels are the codes the display catalog of one generation published, and
/// they are what a tile is written with. The terms are the canonical values the
/// registry of that same generation carries, and they are what decides which role
/// each asset plays: a rendered name is presentation and never a business rule.
#[derive(Clone, Debug)]
pub struct RouteTickerAnswer {
    pub listings: Vec<RouteListing>,
    pub asset_labels: AHashMap<AssetId, SmolStr>,
    pub asset_terms: AHashMap<AssetId, AssetTerms>,
    pub tickers: Vec<ListingTicker>,
}

/// The notional role one canonical asset plays on the map.
///
/// Only a token the venue issued itself under a verified native code is dollar
/// money this map may sum; the same code held by a currency, by a chain contract
/// or by a different family is not the same money.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeatMapNotionalClass {
    /// A native-issued token whose verified code is USDT.
    AcceptedUsdt,
    /// A native-issued token whose verified code is USDC.
    AcceptedUsdc,
    /// Anything else: not a money this map may add up.
    Other,
}

/// The base role one canonical asset plays on the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeatMapBaseClass {
    /// The asset is itself a dollar, so a pair based on it is not a market.
    Excluded,
    /// The asset may be the base of a tile.
    Included,
}

/// The two roles one canonical asset plays, each decided on its own.
///
/// The roles never overwrite each other: USDC is an accepted notional *and* an
/// excluded base at the same time, and a filter that read one role in place of the
/// other would either drop every USDC-quoted pair or draw a dollar as a market.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeatMapAssetClassification {
    /// Which money role the asset plays.
    notional: HeatMapNotionalClass,
    /// Which base role the asset plays.
    base: HeatMapBaseClass,
}

/// Classifies one canonical asset for the map's two independent roles.
///
/// The answer is a function of the canonical terms alone, so no display code and no
/// label can move it: a token a venue issues and a chain token of the same code
/// classify identically.
pub(crate) fn classify_heat_map_asset(terms: &AssetTerms) -> HeatMapAssetClassification {
    HeatMapAssetClassification {
        notional: match terms {
            AssetTerms::CryptoToken(terms) => match terms.origin.token() {
                TokenReference::Native { code } if code.as_str() == USDT_CODE => {
                    HeatMapNotionalClass::AcceptedUsdt
                }
                TokenReference::Native { code } if code.as_str() == USDC_CODE => {
                    HeatMapNotionalClass::AcceptedUsdc
                }
                _ => HeatMapNotionalClass::Other,
            },
            AssetTerms::Currency(_) | AssetTerms::Security { .. } | AssetTerms::Commodity(_) => {
                HeatMapNotionalClass::Other
            }
        },
        base: match native_code(terms) {
            Some(code) if EXCLUDED_BASE_CODES.contains(&code) => HeatMapBaseClass::Excluded,
            _ => HeatMapBaseClass::Included,
        },
    }
}

/// The canonical native code of one asset, when its own terms state one.
///
/// A currency states its ISO 4217 code and a token states the code its origin
/// verified; a contract-referenced token, a security and a commodity state no
/// native code, because an address, an issuer and a venue namespace are not one.
fn native_code(terms: &AssetTerms) -> Option<&str> {
    match terms {
        AssetTerms::Currency(terms) => Some(terms.code.as_str()),
        AssetTerms::CryptoToken(terms) => match terms.origin.token() {
            TokenReference::Native { code } => Some(code.as_str()),
            TokenReference::Contract { .. } => None,
        },
        AssetTerms::Security { .. } | AssetTerms::Commodity(_) => None,
    }
}

/// One ticker that is eligible to become part of a tile.
///
/// It carries only what the fold needs, already narrowed to a spot listing of the
/// route and already stated in the notional its own listing publishes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HeatTicker {
    listing: VenueListingKey,
    instrument: Instrument,
    base_asset_label: SmolStr,
    monotonic_id: Option<i64>,
    open: Price,
    close: Price,
    notional_asset: AssetId,
    notional: Quantity,
}

/// One tile: what it is called, how big it is, and what colour it takes.
///
/// The labels are formatted when the tile is built rather than when it is drawn:
/// a frame that only re-lays out the same entries does not re-format anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HeatMapEntry {
    pub instrument: Instrument,
    pub base_asset_label: SmolStr,
    /// The asset the summed money is denominated in, which is what makes two
    /// tiles of one instrument two tiles rather than one.
    pub notional_asset: AssetId,
    pub notional: Quantity,
    pub notional_label: SmolStr,
    pub direction: PriceDirection,
}

/// The tile being accumulated for one (instrument, notional asset) pair.
struct CombinedTicker {
    instrument: Instrument,
    base_asset_label: SmolStr,
    notional_asset: AssetId,
    notional: Quantity,
    representative_notional: Quantity,
    direction: PriceDirection,
}

/// Why a set of venue answers produced no map.
///
/// The `Display` text of no arm names an identity: the message a reader sees
/// never renders the canonical `InstrumentId` or `AssetId` digest, so an arm states
/// what went wrong and keeps the typed value in its own field for whoever handles the
/// failure. The user-visible wording of each arm is a localized key of the widget.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(super) enum HeatMapModelError {
    #[error("the combined notional is outside the supported range")]
    NotionalOverflow { instrument: Box<Instrument> },
    /// A base asset the generation's own registry carries and its catalog did not name.
    ///
    /// The publication gate refuses a batch whose published name needs a code an asset does
    /// not have, so a base a listed market references always has one: this arm is the
    /// display invariant itself failing rather than a market without a name. It is refused as a typed error — and the whole map with it — because the
    /// alternatives are both worse: a map that silently left the market out would be
    /// a wrong ranking that looks right, and a tile written under a name this widget
    /// derived itself would be a second naming rule free to disagree with the
    /// publication.
    #[error(
        "the published catalog names no code for a base asset the generation's registry carries"
    )]
    AssetNotNamed { asset: Box<AssetId> },
}

/// Narrows one route's answer to the spot readings that can become a tile.
///
/// A reading is kept only when all of the following hold, and a reading that fails
/// any of them contributes nothing rather than being read as something else:
///
/// - the route's published catalog holds the listing, and the listing is a spot
///   listing — the map is a spot map;
/// - the ticker states a traded notional, in the `Notional` unit its listing
///   publishes: money is never read out of an asset quantity;
/// - the catalog registered the canonical terms of both the notional asset and the
///   instrument's base asset, and the two roles those terms state accept the
///   reading: the notional must be a verified USDT or USDC token, and the base must
///   not itself be a dollar.
///
/// Every one of those is a fact of the *terms*. The display label of the base asset
/// is read afterwards, to write the tile with, and it is never a reason to keep or
/// drop a reading: a reading whose base asset has a registered code is a tile, and
/// the code the catalog published for it is what the tile reads.
///
/// The two ways a reading can be without a label are therefore told apart, and only
/// one of them is a failure of this map:
///
/// - **No canonical terms**: the asset is not one this generation registered, so the
///   reading is outside the map's scope and contributes nothing — a reading whose
///   money or base the map cannot classify is not a market it can show, and the
///   alternative would be to guess a role from a venue's answer.
/// - **Terms without a published code**: the generation's registry carries the asset
///   and its catalog published no code for it, which the publication gate keeps out of every
///   published generation. It is refused with [`HeatMapModelError::AssetNotNamed`]
///   rather than dropped, because a map quietly missing a market is a wrong answer
///   that reads as a right one.
fn spot_tickers(answer: RouteTickerAnswer) -> Result<Vec<HeatTicker>, HeatMapModelError> {
    let RouteTickerAnswer {
        listings,
        asset_labels,
        asset_terms,
        tickers,
    } = answer;

    let mut spots: AHashMap<VenueListingKey, Instrument> = AHashMap::with_capacity(listings.len());
    for listing in listings {
        if listing.instrument.kind() == InstrumentKind::Spot {
            spots.insert(listing.listing, listing.instrument);
        }
    }

    let mut readings = Vec::with_capacity(tickers.len());
    for ticker in tickers {
        let Some(instrument) = spots.get(&ticker.listing) else {
            continue;
        };
        let Some(notional) = ticker.traded_notional.as_ref().and_then(traded_notional) else {
            continue;
        };
        let Some(notional_asset) = notional.unit.asset().cloned() else {
            continue;
        };
        let Some(base_asset) = spot_base_asset(instrument) else {
            continue;
        };
        let (Some(notional_terms), Some(base_terms)) = (
            asset_terms.get(&notional_asset),
            asset_terms.get(&base_asset),
        ) else {
            continue;
        };
        if classify_heat_map_asset(notional_terms).notional == HeatMapNotionalClass::Other
            || classify_heat_map_asset(base_terms).base == HeatMapBaseClass::Excluded
        {
            continue;
        }
        let base_asset_label =
            asset_labels
                .get(&base_asset)
                .ok_or_else(|| HeatMapModelError::AssetNotNamed {
                    asset: Box::new(base_asset.clone()),
                })?;
        readings.push(HeatTicker {
            listing: ticker.listing,
            instrument: instrument.clone(),
            base_asset_label: base_asset_label.clone(),
            monotonic_id: ticker.monotonic_id,
            open: ticker.open.value,
            close: ticker.close.value,
            notional_asset,
            notional: notional.value,
        });
    }
    Ok(readings)
}

/// Reads one ticker's money, which is only ever its own declared notional.
fn traded_notional(quantity: &MarketQuantity) -> Option<MarketQuantity> {
    matches!(quantity.unit, QuantityUnit::Notional { .. }).then(|| quantity.clone())
}

/// The asset a spot instrument is traded in.
///
/// An outcome, derivative or security instrument is not a spot pair this map can
/// read a base asset from, so it is left out rather than being read as a pair.
fn spot_base_asset(instrument: &Instrument) -> Option<AssetId> {
    let InstrumentTerms::Spot(spec) = instrument.terms() else {
        return None;
    };
    Some(spec.base().clone())
}

/// Folds one venue's answers into the tiles the map shows.
///
/// Every answer belongs to the same venue — that is what the caller promises by
/// asking one venue — so a reading found on two scopes of it is one market read
/// twice, and the fold sums them. Nothing here can see another venue's answer, so
/// no tile can be a number two venues agreed on.
///
/// `previous` is the map the frame before is drawn from. When the new tiles are
/// equal to it the previous allocation is handed back untouched, and the `bool`
/// says whether anything moved — a map that is re-fetched every ten minutes and
/// usually differs in the last digit is not worth a frame, and comparing here is
/// what keeps that decision out of the drawing code.
pub(super) fn combine_market_tickers(
    ticker_groups: Vec<RouteTickerAnswer>,
    previous: Option<&Arc<[HeatMapEntry]>>,
) -> Result<(Arc<[HeatMapEntry]>, bool), HeatMapModelError> {
    let ticker_capacity = ticker_groups.iter().map(|group| group.tickers.len()).sum();
    let mut by_instrument: HashMap<
        (market_participants_model::settlement::InstrumentId, AssetId),
        CombinedTicker,
    > = HashMap::with_capacity(ticker_capacity);
    for group in ticker_groups {
        let mut tickers = spot_tickers(group)?;
        // A venue that repeats a listing answers with the same listing several
        // times over its own recent history; the newest of them is the one that is
        // still true, and the ordering below is total so that "newest" does not
        // depend on the order the venue happened to send them in.
        tickers.sort_by(compare_ticker_freshness);
        tickers.dedup_by(|left, right| left.listing == right.listing);
        for ticker in tickers {
            let key = (*ticker.instrument.id(), ticker.notional_asset.clone());
            let entry = by_instrument.entry(key).or_insert_with(|| CombinedTicker {
                instrument: ticker.instrument.clone(),
                base_asset_label: ticker.base_asset_label.clone(),
                notional_asset: ticker.notional_asset.clone(),
                notional: Quantity::ZERO,
                representative_notional: Quantity::ZERO,
                direction: price_direction(&ticker),
            });
            let is_representative = ticker.notional > entry.representative_notional;
            entry.notional = entry.notional.checked_add(ticker.notional).ok_or_else(|| {
                HeatMapModelError::NotionalOverflow {
                    instrument: Box::new(entry.instrument.clone()),
                }
            })?;
            if is_representative {
                entry.representative_notional = ticker.notional;
                entry.direction = price_direction(&ticker);
                entry.instrument = ticker.instrument;
            }
        }
    }
    let mut entries = by_instrument
        .into_values()
        .map(|ticker| HeatMapEntry {
            base_asset_label: ticker.base_asset_label,
            notional_label: format_compact_notional(ticker.notional),
            notional_asset: ticker.notional_asset,
            instrument: ticker.instrument,
            notional: ticker.notional,
            direction: ticker.direction,
        })
        .collect::<Vec<_>>();
    entries.sort_by(compare_entry_rank);
    entries.truncate(TILE_LIMIT);
    if let Some(previous) = previous
        && previous.as_ref() == entries.as_slice()
    {
        return Ok((previous.clone(), false));
    }
    Ok((entries.into(), true))
}

fn price_direction(ticker: &HeatTicker) -> PriceDirection {
    match ticker.close.cmp(&ticker.open) {
        Ordering::Less => PriceDirection::Down,
        Ordering::Equal => PriceDirection::Flat,
        Ordering::Greater => PriceDirection::Up,
    }
}

/// Orders two readings of the same listing from newest to oldest.
///
/// Every field is compared, so two readings that differ only in their prices
/// still have a defined winner: the merge has to be a function of the answers and
/// not of the order they arrived in, or the same market would produce two
/// different maps.
fn compare_ticker_freshness(left: &HeatTicker, right: &HeatTicker) -> Ordering {
    left.listing
        .cmp(&right.listing)
        .then_with(|| right.monotonic_id.cmp(&left.monotonic_id))
        .then_with(|| right.notional.cmp(&left.notional))
        .then_with(|| right.close.cmp(&left.close))
        .then_with(|| right.open.cmp(&left.open))
}

/// Ranks two tiles: the largest first, and equal volumes by name so that a tie is
/// not decided by the hash map's iteration order.
fn compare_entry_rank(left: &HeatMapEntry, right: &HeatMapEntry) -> Ordering {
    right
        .notional
        .cmp(&left.notional)
        .then_with(|| left.base_asset_label.cmp(&right.base_asset_label))
        .then_with(|| left.notional_asset.cmp(&right.notional_asset))
        .then_with(|| left.instrument.id().cmp(right.instrument.id()))
}

/// Writes a notional in the largest unit that still reads as a number.
///
/// Two decimals below ten, one below a hundred, none above it, and a rounding
/// that is allowed to carry into the next unit: `999_999` is `1M`, not `1000K`.
/// Trailing zeros are dropped, so the tile reads `1.25M` rather than `1.25M` with
/// a tail of digits that is always zero.
pub(super) fn format_compact_notional(value: Quantity) -> SmolStr {
    let raw = value.inner();
    let mut unit_index = DISPLAY_UNITS
        .iter()
        .position(|(multiplier, _)| raw >= FIXED_SCALE * multiplier)
        .unwrap_or(DISPLAY_UNITS.len() - 1);

    loop {
        let (multiplier, suffix) = DISPLAY_UNITS[unit_index];
        let denominator = FIXED_SCALE * multiplier;
        let whole = raw / denominator;
        let decimals = if whole >= 100 {
            0
        } else if whole >= 10 {
            1
        } else {
            2
        };
        let factor = 10_u128.pow(decimals);
        let remainder = raw % denominator;
        let mut fraction = (remainder * factor + denominator / 2) / denominator;
        let mut rounded_whole = whole;
        if fraction == factor {
            rounded_whole += 1;
            fraction = 0;
        }
        if rounded_whole >= 1_000 && unit_index > 0 {
            unit_index -= 1;
            continue;
        }

        if decimals == 0 || fraction == 0 {
            return format_smolstr!("{rounded_whole}{suffix}");
        }
        let fraction_text = format_smolstr!("{fraction:0width$}", width = decimals as usize);
        let fraction_text = fraction_text.trim_end_matches('0');
        if fraction_text.is_empty() {
            return format_smolstr!("{rounded_whole}{suffix}");
        }
        return format_smolstr!("{rounded_whole}.{fraction_text}{suffix}");
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use market_participants_connector::catalog::test_support;
    use market_participants_model::{
        listing::{route::VenueRoute, venue::PriceBasis},
        market::{price::MarketPrice, units::MarketQuantity},
    };

    use super::*;

    fn binance() -> VenueRoute {
        test_support::route("binance", "spot")
    }

    fn okx() -> VenueRoute {
        test_support::route("okx", "spot")
    }

    fn quantity(value: &str) -> Quantity {
        Quantity::from_str(value).expect("test quantity should parse")
    }

    fn price(value: &str) -> Price {
        Price::from_str(value).expect("test price should parse")
    }

    fn market_price(value: &str, quote: &str) -> MarketPrice {
        MarketPrice {
            value: price(value),
            quote_asset: test_support::token("fixture", quote),
            basis: PriceBasis::PerAssetUnit,
        }
    }

    /// One spot reading of one route, with the asset codes its catalog knows.
    #[derive(Clone)]
    struct Reading {
        listing: RouteListing,
        ticker: ListingTicker,
        base_code: SmolStr,
        quote_code: SmolStr,
    }

    fn reading(route: &VenueRoute, base: &str, quote: &str, notional: &str) -> Reading {
        let instrument = test_support::spot_token("fixture", base, quote);
        let key = test_support::listing_key(route, &format!("{base}{quote}"));
        Reading {
            listing: RouteListing {
                listing: key.clone(),
                instrument,
            },
            ticker: ListingTicker {
                listing: key,
                monotonic_id: Some(1),
                open: market_price("10", quote),
                high: Some(market_price("12", quote)),
                low: Some(market_price("9", quote)),
                close: market_price("11", quote),
                traded_quantity: None,
                traded_notional: Some(
                    MarketQuantity::amount(
                        quantity(notional),
                        QuantityUnit::Notional {
                            asset: test_support::token("fixture", quote),
                        },
                    )
                    .expect("a notional amount is money"),
                ),
            },
            base_code: SmolStr::new(base),
            quote_code: SmolStr::new(quote),
        }
    }

    /// One route's answer, with every label and every term the catalog would have
    /// registered for the assets the readings name.
    fn answer(readings: Vec<Reading>) -> RouteTickerAnswer {
        let mut listings = Vec::with_capacity(readings.len());
        let mut asset_labels = AHashMap::with_capacity(readings.len() * 2);
        let mut asset_terms = AHashMap::with_capacity(readings.len() * 2);
        let mut tickers = Vec::with_capacity(readings.len());
        for reading in readings {
            for code in [&reading.base_code, &reading.quote_code] {
                asset_labels.insert(test_support::token("fixture", code.as_str()), code.clone());
                asset_terms.insert(
                    test_support::token("fixture", code.as_str()),
                    AssetTerms::CryptoToken(test_support::token_terms("fixture", code.as_str())),
                );
            }
            listings.push(reading.listing);
            tickers.push(reading.ticker);
        }
        RouteTickerAnswer {
            listings,
            asset_labels,
            asset_terms,
            tickers,
        }
    }

    /// The typed classification of one fixture token code.
    fn classification(code: &str) -> HeatMapAssetClassification {
        classify_heat_map_asset(&AssetTerms::CryptoToken(test_support::token_terms(
            "fixture", code,
        )))
    }

    /// The base labels of one folded venue, in the order the map ranks them.
    fn labels(entries: &Arc<[HeatMapEntry]>) -> Vec<SmolStr> {
        entries
            .iter()
            .map(|entry| entry.base_asset_label.clone())
            .collect()
    }

    #[test]
    fn combine_market_tickers_should_sum_one_venues_scopes_and_use_canonical_tie_breaks() {
        let (entries, changed) = combine_market_tickers(
            vec![
                answer(vec![
                    reading(&binance(), "BBB", "USDT", "70"),
                    reading(&binance(), "AAA", "USDT", "30"),
                ]),
                answer(vec![
                    reading(
                        &test_support::route("binance", "usd-m-perp"),
                        "CCC",
                        "USDT",
                        "70",
                    ),
                    reading(
                        &test_support::route("binance", "usd-m-perp"),
                        "AAA",
                        "USDT",
                        "40",
                    ),
                ]),
            ],
            None,
        )
        .expect("volumes should combine");

        assert!(changed);
        assert_eq!(
            entries
                .iter()
                .map(|entry| (
                    entry.base_asset_label.clone(),
                    entry.notional,
                    entry.direction,
                ))
                .collect::<Vec<_>>(),
            [
                (
                    SmolStr::new_static("AAA"),
                    quantity("70"),
                    PriceDirection::Up
                ),
                (
                    SmolStr::new_static("BBB"),
                    quantity("70"),
                    PriceDirection::Up
                ),
                (
                    SmolStr::new_static("CCC"),
                    quantity("70"),
                    PriceDirection::Up
                ),
            ]
        );
    }

    /// The whole point of folding one venue at a time: `AAA` is one instrument
    /// trading on two venues, and the two readings are two numbers about two order
    /// books rather than one number about a market nobody quotes.
    #[test]
    fn the_same_instrument_on_two_venues_should_be_two_numbers() {
        let (binance_map, _) = combine_market_tickers(
            vec![answer(vec![reading(&binance(), "AAA", "USDT", "70")])],
            None,
        )
        .expect("one venue's reading should combine");
        let (okx_map, _) = combine_market_tickers(
            vec![answer(vec![reading(&okx(), "AAA", "USDT", "30")])],
            None,
        )
        .expect("one venue's reading should combine");

        assert_eq!(labels(&binance_map), [SmolStr::new_static("AAA")]);
        assert_eq!(labels(&okx_map), [SmolStr::new_static("AAA")]);
        assert_eq!(binance_map[0].notional, quantity("70"));
        assert_eq!(
            okx_map[0].notional,
            quantity("30"),
            "each map carries its own venue's money and never the other's"
        );
    }

    #[test]
    fn combine_market_tickers_should_choose_the_largest_individual_source() {
        let largest = reading(&binance(), "AAA", "USDT", "100");
        let mut smaller = reading(
            &test_support::route("binance", "usd-m-perp"),
            "AAA",
            "USDT",
            "60",
        );
        smaller.ticker.close = market_price("9", "USDT");

        let (entries, _) =
            combine_market_tickers(vec![answer(vec![largest]), answer(vec![smaller])], None)
                .expect("market volumes should combine");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].notional, quantity("160"));
        assert_eq!(entries[0].direction, PriceDirection::Up);
    }

    #[test]
    fn combine_market_tickers_should_only_accept_exact_usdt_and_usdc_notionals() {
        let (entries, _) = combine_market_tickers(
            vec![answer(vec![
                reading(&binance(), "AAA", "USDT", "10"),
                reading(&binance(), "BBB", "USDC", "20"),
                reading(&binance(), "CCC", "USDT0", "100"),
                reading(&binance(), "DDD", "USDH", "100"),
            ])],
            None,
        )
        .expect("supported quote volumes should combine");

        assert_eq!(
            labels(&entries),
            [SmolStr::new_static("BBB"), SmolStr::new_static("AAA")]
        );
    }

    #[test]
    fn combine_market_tickers_should_drop_instruments_that_are_themselves_a_dollar() {
        let (entries, _) = combine_market_tickers(
            vec![answer(vec![
                reading(&binance(), "BTC", "USDT", "10"),
                reading(&binance(), "USDC", "USDT", "1000"),
                reading(&binance(), "FDUSD", "USDT", "500"),
            ])],
            None,
        )
        .expect("tradable assets should combine");

        assert_eq!(labels(&entries), [SmolStr::new_static("BTC")]);
    }

    /// The two roles of one asset are independent, and USDC is the asset that
    /// proves it: the same token is an accepted notional *and* an excluded base.
    #[test]
    fn usdc_should_be_an_accepted_notional_and_an_excluded_base_at_once() {
        assert_eq!(
            classification("USDC"),
            HeatMapAssetClassification {
                notional: HeatMapNotionalClass::AcceptedUsdc,
                base: HeatMapBaseClass::Excluded,
            }
        );
    }

    /// The other half of the same pair: USDT is accepted money and a tradable base.
    #[test]
    fn usdt_should_be_an_accepted_notional_and_an_included_base() {
        assert_eq!(
            classification("USDT"),
            HeatMapAssetClassification {
                notional: HeatMapNotionalClass::AcceptedUsdt,
                base: HeatMapBaseClass::Included,
            }
        );
    }

    /// A fiat dollar is excluded from the base role and is never accepted money:
    /// `AssetId::Currency` is an ISO code, and an ISO code is not a token.
    #[test]
    fn a_currency_dollar_should_be_excluded_and_never_an_accepted_notional() {
        assert_eq!(
            classify_heat_map_asset(&AssetTerms::Currency(test_support::currency_terms("USD"))),
            HeatMapAssetClassification {
                notional: HeatMapNotionalClass::Other,
                base: HeatMapBaseClass::Excluded,
            }
        );
    }

    /// The same run, read through the filter: an accepted USDC notional produces a
    /// tile, and a USDC base produces none — because the two roles are read apart.
    #[test]
    fn a_usdc_notional_should_be_accepted_while_a_usdc_base_should_be_excluded() {
        let (entries, _) = combine_market_tickers(
            vec![answer(vec![
                reading(&binance(), "BTC", "USDC", "10"),
                reading(&binance(), "USDC", "USDT", "1000"),
            ])],
            None,
        )
        .expect("the readings should combine");

        assert_eq!(labels(&entries), [SmolStr::new_static("BTC")]);
        assert_eq!(
            entries[0].notional_asset,
            test_support::token("fixture", "USDC")
        );
    }

    /// A currency quoted reading is not dollar money, however its label reads.
    #[test]
    fn a_currency_quoted_reading_should_not_be_accepted_as_notional_money() {
        let usd = test_support::currency("USD");
        let mut reading = reading(&binance(), "BTC", "USD", "10");
        reading.ticker.traded_notional = Some(
            MarketQuantity::amount(
                quantity("10"),
                QuantityUnit::Notional { asset: usd.clone() },
            )
            .expect("a notional amount is money"),
        );
        let mut answer = answer(vec![reading]);
        answer
            .asset_labels
            .insert(usd.clone(), SmolStr::new_static("USD"));
        answer.asset_terms.insert(
            usd,
            AssetTerms::Currency(test_support::currency_terms("USD")),
        );

        let (entries, _) = combine_market_tickers(vec![answer], None)
            .expect("an answer whose money is a currency should combine to nothing");

        assert!(entries.is_empty());
    }

    /// Filtering reads the canonical terms, so the code the catalog published for an
    /// asset cannot move a reading in or out of the map.
    #[test]
    fn filtering_should_be_independent_of_the_published_asset_code() {
        let readings = vec![reading(&binance(), "BTC", "USDT", "10")];

        let plain = answer(readings.clone());
        let mut relabelled = answer(readings);
        relabelled.asset_labels.insert(
            test_support::token("fixture", "USDT"),
            SmolStr::new_static("USDT0"),
        );
        relabelled.asset_labels.insert(
            test_support::token("fixture", "BTC"),
            SmolStr::new_static("WBTC"),
        );

        let (plain_entries, _) =
            combine_market_tickers(vec![plain], None).expect("a plain code should combine");
        let (relabelled_entries, _) = combine_market_tickers(vec![relabelled], None)
            .expect("another published code should combine");

        assert_eq!(plain_entries.len(), 1);
        assert_eq!(relabelled_entries.len(), 1);
        assert_eq!(plain_entries[0].notional, relabelled_entries[0].notional);
        assert_eq!(
            plain_entries[0].notional_asset,
            relabelled_entries[0].notional_asset
        );
        assert_eq!(relabelled_entries[0].base_asset_label.as_str(), "WBTC");
    }

    /// The other half of that independence: a label that spells an accepted code
    /// does not make an asset accepted money, because the label is not the rule.
    #[test]
    fn a_rendered_label_should_not_accept_a_notional_the_terms_reject() {
        let mut answer = answer(vec![reading(&binance(), "BTC", "USDT0", "10")]);
        answer.asset_labels.insert(
            test_support::token("fixture", "USDT0"),
            SmolStr::new_static("USDT"),
        );

        let (entries, _) = combine_market_tickers(vec![answer], None)
            .expect("a rejected notional should combine to nothing");

        assert!(entries.is_empty());
    }

    /// One instrument is one tile however many readings of it a route answers
    /// with, and the map is bounded by the tile limit like any other ranking.
    #[test]
    fn combine_market_tickers_should_rank_one_venues_readings_and_keep_the_top_twenty() {
        let readings = (0..=TILE_LIMIT)
            .map(|index| {
                reading(
                    &binance(),
                    &format!("A{index:02}"),
                    "USDT",
                    &format!("{}", 100 - index),
                )
            })
            .collect::<Vec<_>>();

        let (entries, _) =
            combine_market_tickers(vec![answer(readings)], None).expect("volumes should combine");

        assert_eq!(
            labels(&entries),
            (0..TILE_LIMIT)
                .map(|index| format_smolstr!("A{index:02}"))
                .collect::<Vec<_>>(),
            "the map shows the {TILE_LIMIT} largest readings and never one more"
        );
    }

    #[test]
    fn combine_market_tickers_should_keep_the_largest_duplicate_per_route() {
        let (entries, _) = combine_market_tickers(
            vec![answer(vec![
                reading(&binance(), "DUP", "USDT", "10"),
                reading(&binance(), "DUP", "USDT", "30"),
                reading(
                    &test_support::route("binance", "usd-m-perp"),
                    "DUP",
                    "USDT",
                    "5",
                ),
                reading(
                    &test_support::route("binance", "usd-m-perp"),
                    "DUP",
                    "USDT",
                    "20",
                ),
            ])],
            None,
        )
        .expect("volumes should combine");

        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.base_asset_label.clone(), entry.notional))
                .collect::<Vec<_>>(),
            [(SmolStr::new_static("DUP"), quantity("50"))]
        );
    }

    #[test]
    fn combine_market_tickers_should_keep_the_newest_duplicate_before_ranking_by_volume() {
        let mut old = reading(&binance(), "DUP", "USDT", "1000");
        old.ticker.monotonic_id = Some(10);
        let mut new = reading(&binance(), "DUP", "USDT", "1");
        new.ticker.monotonic_id = Some(11);

        let (entries, _) = combine_market_tickers(vec![answer(vec![old, new])], None)
            .expect("newest ticker should be retained");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].notional, quantity("1"));
    }

    #[test]
    fn combine_market_tickers_should_resolve_equal_sequences_deterministically() {
        let mut lower = reading(&binance(), "DUP", "USDT", "10");
        lower.ticker.monotonic_id = Some(7);
        lower.ticker.close = market_price("9", "USDT");
        let mut higher = reading(&binance(), "DUP", "USDT", "10");
        higher.ticker.monotonic_id = Some(7);
        higher.ticker.close = market_price("11", "USDT");

        let forward =
            combine_market_tickers(vec![answer(vec![lower.clone(), higher.clone()])], None)
                .expect("equal-sequence duplicates should combine");
        let reversed = combine_market_tickers(vec![answer(vec![higher, lower])], None)
            .expect("reordered equal-sequence duplicates should combine");

        assert_eq!(forward.0, reversed.0);
        assert_eq!(forward.0[0].direction, PriceDirection::Up);
    }

    #[test]
    fn combine_market_tickers_should_reuse_equal_snapshot_allocation() {
        let readings = vec![
            reading(&binance(), "AAA", "USDT", "10"),
            reading(&binance(), "BBB", "USDT", "20"),
            reading(
                &test_support::route("binance", "usd-m-perp"),
                "AAA",
                "USDT",
                "15",
            ),
            reading(
                &test_support::route("binance", "usd-m-perp"),
                "CCC",
                "USDT",
                "5",
            ),
        ];
        let (previous, _) =
            combine_market_tickers(vec![answer(readings.clone()), answer(Vec::new())], None)
                .expect("the first volumes should combine");

        let mut reordered = readings;
        reordered.reverse();
        let (next, changed) =
            combine_market_tickers(vec![answer(Vec::new()), answer(reordered)], Some(&previous))
                .expect("reordered volumes should combine");

        assert!(!changed);
        assert!(Arc::ptr_eq(&previous, &next));
    }

    #[test]
    fn combine_market_tickers_should_report_notional_overflow() {
        let mut huge = reading(&binance(), "BTC", "USDT", "1");
        huge.ticker.traded_notional = Some(
            MarketQuantity::amount(
                Quantity::from_inner(u128::MAX),
                QuantityUnit::Notional {
                    asset: test_support::token("fixture", "USDT"),
                },
            )
            .expect("the fixture notional should be money"),
        );
        let instrument = huge.listing.instrument.clone();

        let error = combine_market_tickers(
            vec![
                answer(vec![huge]),
                answer(vec![reading(
                    &test_support::route("binance", "usd-m-perp"),
                    "BTC",
                    "USDT",
                    "1",
                )]),
            ],
            None,
        )
        .expect_err("overflow should be reported");

        assert_eq!(
            error,
            HeatMapModelError::NotionalOverflow {
                instrument: Box::new(instrument),
            }
        );
    }

    /// Two monies are two numbers: the same instrument read in USDT and in USDC
    /// stays two rows, and neither of them claims the other's readings.
    #[test]
    fn notionals_of_different_assets_should_not_be_merged() {
        let (entries, _) = combine_market_tickers(
            vec![answer(vec![
                reading(&binance(), "BTC", "USDT", "10"),
                reading(
                    &test_support::route("binance", "usd-m-perp"),
                    "BTC",
                    "USDC",
                    "20",
                ),
            ])],
            None,
        )
        .expect("the readings should combine");

        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.notional)
                .collect::<Vec<_>>(),
            [quantity("20"), quantity("10")]
        );
    }

    /// A ticker that states no money contributes nothing: the map is sized by
    /// notional, and an asset quantity is not one.
    #[test]
    fn a_reading_without_a_declared_notional_should_contribute_nothing() {
        let mut ticker = reading(&binance(), "BTC", "USDT", "10");
        ticker.ticker.traded_notional = None;

        let (entries, _) = combine_market_tickers(vec![answer(vec![ticker])], None)
            .expect("an answer without money should combine to an empty map");

        assert!(entries.is_empty());
    }

    /// The map is a spot map: a listing the route's catalog does not publish is
    /// not a row, however well formed its ticker is.
    #[test]
    fn a_listing_the_catalog_does_not_publish_should_be_left_out() {
        let mut answer = answer(vec![reading(&binance(), "BTC", "USDT", "10")]);
        answer.listings.clear();

        let (entries, _) = combine_market_tickers(vec![answer], None)
            .expect("an answer whose catalog holds nothing should combine to an empty map");

        assert!(entries.is_empty());
    }

    /// The display code of the base asset writes the tile and is never a filter: a
    /// reading that passes every canonical test is a row, and a base asset of the
    /// generation's own registry that its catalog published no code for is a display
    /// invariant failing rather than a market silently left off the map.
    ///
    /// The case is unreachable through a real read — a generation publishes a code for every
    /// asset a generation's registry carries, and the labels and the terms are read
    /// from one generation — so this arm is the map's own assertion rather than a path
    /// a venue can take. It refuses the whole map deliberately: a map quietly missing
    /// a market is a wrong ranking that reads as a right one, and a tile written under
    /// a name this widget invented would be a second naming rule for an asset the
    /// publication already decided the name of. The *other* way a base can be
    /// without a code — an asset the generation's registry never carried — is a market
    /// outside this map's scope, and it is covered where the labels are read.
    #[test]
    fn a_nameless_base_asset_should_be_refused_rather_than_dropped() {
        let mut answer = answer(vec![reading(&binance(), "BTC", "USDT", "10")]);
        let base = test_support::token("fixture", "BTC");
        answer.asset_labels.remove(&base);

        let error = combine_market_tickers(vec![answer], None)
            .expect_err("a nameless base asset is the display invariant failing");

        assert_eq!(
            error,
            HeatMapModelError::AssetNotNamed {
                asset: Box::new(base)
            }
        );
    }

    /// Two scopes of one venue are one market read twice, so their readings are
    /// two tiles rather than one number: what the fold adds up is an instrument's
    /// money, never a venue's.
    #[test]
    fn two_scopes_of_one_venue_should_produce_two_tiles() {
        let (entries, _) = combine_market_tickers(
            vec![
                answer(vec![reading(&binance(), "BTC", "USDT", "10")]),
                answer(vec![reading(
                    &test_support::route("binance", "usd-m-perp"),
                    "ETH",
                    "USDT",
                    "20",
                )]),
            ],
            None,
        )
        .expect("the readings should combine");

        assert_eq!(
            labels(&entries),
            [SmolStr::new_static("ETH"), SmolStr::new_static("BTC")]
        );
    }

    #[test]
    fn format_compact_notional_should_use_the_largest_short_unit() {
        let cases = [
            ("999", "999"),
            ("1000", "1K"),
            ("12500", "12.5K"),
            ("1250000", "1.25M"),
            ("1250000000", "1.25B"),
            ("1250000000000", "1.25T"),
            ("1250000000000000", "1.25Q"),
        ];

        for (value, expected) in cases {
            assert_eq!(format_compact_notional(quantity(value)), expected);
        }
    }

    #[test]
    fn format_compact_notional_should_promote_a_rounded_unit() {
        assert_eq!(format_compact_notional(quantity("999999")), "1M");
    }

    #[test]
    fn format_compact_notional_should_drop_a_fraction_that_rounds_to_none() {
        assert_eq!(format_compact_notional(quantity("1000.004")), "1K");
        assert_eq!(format_compact_notional(quantity("1250")), "1.25K");
        assert_eq!(format_compact_notional(quantity("1255")), "1.26K");
    }
}
