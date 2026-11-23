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

pub(super) const TILE_LIMIT: usize = 20;

const USDT_CODE: &str = "USDT";

const USDC_CODE: &str = "USDC";

const EXCLUDED_BASE_CODES: [&str; 6] = ["U", "USDC", "USD1", "USD", "FDUSD", "USDS"];

const FIXED_SCALE: u128 = 1_000_000_000_000_000_000;

const DISPLAY_UNITS: [(u128, &str); 6] = [
    (1_000_000_000_000_000, "Q"),
    (1_000_000_000_000, "T"),
    (1_000_000_000, "B"),
    (1_000_000, "M"),
    (1_000, "K"),
    (1, ""),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PriceDirection {
    Down,
    Flat,
    Up,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteListing {
    pub listing: VenueListingKey,
    pub instrument: Instrument,
}

#[derive(Clone, Debug)]
pub struct RouteTickerAnswer {
    pub listings: Vec<RouteListing>,
    pub asset_labels: AHashMap<AssetId, SmolStr>,
    pub asset_terms: AHashMap<AssetId, AssetTerms>,
    pub tickers: Vec<ListingTicker>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeatMapNotionalClass {
    AcceptedUsdt,
    AcceptedUsdc,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeatMapBaseClass {
    Excluded,
    Included,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeatMapAssetClassification {
    notional: HeatMapNotionalClass,
    base: HeatMapBaseClass,
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HeatMapEntry {
    pub instrument: Instrument,
    pub base_asset_label: SmolStr,
    pub notional_asset: AssetId,
    pub notional: Quantity,
    pub notional_label: SmolStr,
    pub direction: PriceDirection,
}

struct CombinedTicker {
    instrument: Instrument,
    base_asset_label: SmolStr,
    notional_asset: AssetId,
    notional: Quantity,
    representative_notional: Quantity,
    direction: PriceDirection,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(super) enum HeatMapModelError {
    #[error("the combined notional is outside the supported range")]
    NotionalOverflow { instrument: Box<Instrument> },
    #[error(
        "the published catalog names no code for a base asset the generation's registry carries"
    )]
    AssetNotNamed { asset: Box<AssetId> },
}

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

fn traded_notional(quantity: &MarketQuantity) -> Option<MarketQuantity> {
    matches!(quantity.unit, QuantityUnit::Notional { .. }).then(|| quantity.clone())
}

fn spot_base_asset(instrument: &Instrument) -> Option<AssetId> {
    let InstrumentTerms::Spot(spec) = instrument.terms() else {
        return None;
    };
    Some(spec.base().clone())
}

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

fn compare_ticker_freshness(left: &HeatTicker, right: &HeatTicker) -> Ordering {
    left.listing
        .cmp(&right.listing)
        .then_with(|| right.monotonic_id.cmp(&left.monotonic_id))
        .then_with(|| right.notional.cmp(&left.notional))
        .then_with(|| right.close.cmp(&left.close))
        .then_with(|| right.open.cmp(&left.open))
}

fn compare_entry_rank(left: &HeatMapEntry, right: &HeatMapEntry) -> Ordering {
    right
        .notional
        .cmp(&left.notional)
        .then_with(|| left.base_asset_label.cmp(&right.base_asset_label))
        .then_with(|| left.notional_asset.cmp(&right.notional_asset))
        .then_with(|| left.instrument.id().cmp(right.instrument.id()))
}

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

    fn classification(code: &str) -> HeatMapAssetClassification {
        classify_heat_map_asset(&AssetTerms::CryptoToken(test_support::token_terms(
            "fixture", code,
        )))
    }

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

    #[test]
    fn a_reading_without_a_declared_notional_should_contribute_nothing() {
        let mut ticker = reading(&binance(), "BTC", "USDT", "10");
        ticker.ticker.traded_notional = None;

        let (entries, _) = combine_market_tickers(vec![answer(vec![ticker])], None)
            .expect("an answer without money should combine to an empty map");

        assert!(entries.is_empty());
    }

    #[test]
    fn a_listing_the_catalog_does_not_publish_should_be_left_out() {
        let mut answer = answer(vec![reading(&binance(), "BTC", "USDT", "10")]);
        answer.listings.clear();

        let (entries, _) = combine_market_tickers(vec![answer], None)
            .expect("an answer whose catalog holds nothing should combine to an empty map");

        assert!(entries.is_empty());
    }

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
