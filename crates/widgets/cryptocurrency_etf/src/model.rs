//! What an ETF flow snapshot is, and how one service answer becomes one.
//!
//! The service publishes the whole history of every asset it tracks in a single
//! response: four maps of dated rows, one per asset, each row carrying one string
//! per fund. Everything the chart needs is derived here, once, at that boundary,
//! and shared through an `Arc`:
//!
//! - the rows are keyed by date rather than appended, because a map has no order
//!   and a date may be corrected inside the interval it belongs to;
//! - each fund's flow is read in the fund order of the asset, not in the order the
//!   response happened to name it, so a fund keeps its own column;
//! - net assets are *accumulated* over the whole history rather than over the part
//!   a chart is showing, because the number is "how much these funds hold", which
//!   does not restart when the user picks a shorter window.
//!
//! Everything a row contains is validated: a venue answer that does not parse, or
//! that repeats a date, or that is large enough to suggest the endpoint is not
//! what was asked of it, is an error rather than a chart drawn from it.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Arc,
};

use kytrix_proto::types as proto_types;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use thiserror::Error;
use time::{Date, Month};

/// The most dated rows one asset may carry.
///
/// The funds traded for a few years, so this is far beyond any real history: it is
/// the point at which the answer stops being an ETF history and starts being a
/// response this widget should not be holding in memory.
pub(crate) const MAX_ETF_DAYS_PER_ASSET: usize = 16_384;

/// The asset whose ETF complex a chart is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum CryptocurrencyEtfAsset {
    #[default]
    #[serde(rename = "btc")]
    Btc,
    #[serde(rename = "eth")]
    Eth,
    #[serde(rename = "sol")]
    Sol,
    #[serde(rename = "hype")]
    Hyp,
}

impl CryptocurrencyEtfAsset {
    /// Every asset the service publishes, in the order the tabs offer them.
    pub(crate) const ALL: [Self; 4] = [Self::Btc, Self::Eth, Self::Sol, Self::Hyp];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Btc => 0,
            Self::Eth => 1,
            Self::Sol => 2,
            Self::Hyp => 3,
        }
    }

    /// The ticker the tab is labelled with. It is a fund's name, not a
    /// translated word: `HYPE` reads the same in every locale.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Btc => "BTC",
            Self::Eth => "ETH",
            Self::Sol => "SOL",
            Self::Hyp => "HYPE",
        }
    }
}

/// How much of the history a chart shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum CryptocurrencyEtfWindow {
    #[default]
    #[serde(rename = "days_30")]
    Days30,
    #[serde(rename = "days_90")]
    Days90,
    #[serde(rename = "days_180")]
    Days180,
    #[serde(rename = "days_360")]
    Days360,
    #[serde(rename = "all")]
    All,
}

impl CryptocurrencyEtfWindow {
    /// Every window the picker offers, in the order it offers them.
    pub(crate) const ALL: [Self; 5] = [
        Self::Days30,
        Self::Days90,
        Self::Days180,
        Self::Days360,
        Self::All,
    ];

    /// How many of the most recent rows the window keeps, or `None` for all of
    /// them.
    ///
    /// The limit counts *rows*, not calendar days: the funds trade on weekdays
    /// only, and a window of thirty calendar days would show about twenty bars —
    /// a number nobody chose.
    const fn day_limit(self) -> Option<usize> {
        match self {
            Self::Days30 => Some(30),
            Self::Days90 => Some(90),
            Self::Days180 => Some(180),
            Self::Days360 => Some(360),
            Self::All => None,
        }
    }
}

/// Which funds a chart is showing together.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum CryptocurrencyEtfFundSelection {
    /// Every fund of the asset, which is what its total flow is.
    #[default]
    #[serde(rename = "all")]
    All,
    /// One fund, named by its ticker.
    #[serde(rename = "named")]
    Named(SmolStr),
}

/// One trading day of one asset.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CryptocurrencyEtfDay {
    pub date: Date,
    /// One flow per fund, in the asset's own fund order, in millions of dollars.
    pub fund_flows: Arc<[f64]>,
    /// What every fund together moved that day.
    pub total_flow: f64,
    /// What the funds hold after that day, accumulated from the first row.
    pub net_assets: f64,
}

/// The history of one asset.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CryptocurrencyEtfDataset {
    pub asset: CryptocurrencyEtfAsset,
    /// The funds the rows are read in, in the order they were read.
    pub funds: Arc<[SmolStr]>,
    /// The rows, oldest first.
    pub days: Arc<[CryptocurrencyEtfDay]>,
}

impl CryptocurrencyEtfDataset {
    /// The most recent rows a window keeps, borrowed from the same history.
    pub(crate) fn days_for_window(
        &self,
        window: CryptocurrencyEtfWindow,
    ) -> &[CryptocurrencyEtfDay] {
        let start = window
            .day_limit()
            .map_or(0, |limit| self.days.len().saturating_sub(limit));
        &self.days[start..]
    }

    /// Where a fund sits in the row's own fund order.
    pub(crate) fn fund_index(&self, name: &str) -> Option<usize> {
        self.funds.iter().position(|fund| fund == name)
    }

    /// What one row moved, for the funds a chart is showing.
    ///
    /// A named fund the asset does not have has no flow rather than a zero one:
    /// the two are different readings, and only one of them is a number.
    pub(crate) fn flow_for(
        &self,
        day: &CryptocurrencyEtfDay,
        selection: &CryptocurrencyEtfFundSelection,
    ) -> Option<f64> {
        match selection {
            CryptocurrencyEtfFundSelection::All => Some(day.total_flow),
            CryptocurrencyEtfFundSelection::Named(name) => self
                .fund_index(name.as_str())
                .and_then(|index| day.fund_flows.get(index).copied()),
        }
    }
}

/// Every asset's history, which is what one service answer becomes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CryptocurrencyEtfSnapshot {
    /// One history per asset, in [`CryptocurrencyEtfAsset::ALL`] order.
    pub datasets: [Arc<CryptocurrencyEtfDataset>; 4],
}

impl CryptocurrencyEtfSnapshot {
    pub(crate) fn dataset(&self, asset: CryptocurrencyEtfAsset) -> Arc<CryptocurrencyEtfDataset> {
        self.datasets[asset.index()].clone()
    }
}

/// Why an answer could not become a snapshot.
///
/// Every variant names the asset it came from: one malformed row invalidates the
/// whole answer, and a message that did not say which asset it was about would
/// leave the reader looking through four of them.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(crate) enum CryptocurrencyEtfParseError {
    #[error("{asset:?} ETF response contains no dated rows")]
    EmptyAssetMap { asset: CryptocurrencyEtfAsset },
    #[error("invalid {asset:?} ETF trading date `{value}`; expected `YYYY-MM-DD`")]
    InvalidDate {
        asset: CryptocurrencyEtfAsset,
        value: SmolStr,
    },
    #[error("{asset:?} ETF response contains duplicate trading date `{date}`")]
    DuplicateDate {
        asset: CryptocurrencyEtfAsset,
        date: Date,
    },
    #[error("{asset:?} ETF response exceeds the {maximum}-day safety limit")]
    TooManyRows {
        asset: CryptocurrencyEtfAsset,
        maximum: usize,
    },
    #[error("invalid {asset:?} ETF number `{value}` on `{date}` for `{fund}`")]
    InvalidNumber {
        asset: CryptocurrencyEtfAsset,
        date: SmolStr,
        fund: SmolStr,
        value: SmolStr,
    },
    #[error("{asset:?} ETF total flow became non-finite on `{date}`")]
    NonFiniteTotal {
        asset: CryptocurrencyEtfAsset,
        date: Date,
    },
    #[error("{asset:?} ETF net assets became non-finite at `{date}`")]
    NonFiniteAssets {
        asset: CryptocurrencyEtfAsset,
        date: Date,
    },
}

/// One row before the running total is folded into it.
struct ParsedDay {
    fund_flows: Arc<[f64]>,
    total_flow: f64,
}

/// The funds each asset's rows are read in, in the order the wire format names
/// them. A row is a fixed struct on the wire, so the fund list of an asset is a
/// property of the asset rather than of the answer.
const BTC_FUNDS: [&str; 12] = [
    "IBIT", "FBTC", "BITB", "ARKB", "BTCO", "EZBC", "BRRR", "HODL", "BTCW", "MSBT", "GBTC", "BTC",
];
const ETH_FUNDS: [&str; 10] = [
    "ETHA", "ETHB", "FETH", "ETHW", "TETH", "ETHV", "QETH", "EZET", "ETHE", "ETH",
];
const SOL_FUNDS: [&str; 6] = ["BSOL", "VSOL", "FSOL", "TSOL", "SOEZ", "GSOL"];
const HYPE_FUNDS: [&str; 3] = ["BHYP", "THYP", "HYPG"];

/// Reads one service answer into the history of every asset.
///
/// It is all or nothing: an answer that cannot be read for one asset is not used
/// for the other three either, because a snapshot is a single reading of the
/// market and half of one would be a chart that disagrees with itself.
pub(crate) fn parse_etf(
    snapshot: proto_types::CryptocurrencyEtf,
) -> Result<CryptocurrencyEtfSnapshot, CryptocurrencyEtfParseError> {
    let btc = parse_rows(
        CryptocurrencyEtfAsset::Btc,
        &BTC_FUNDS,
        snapshot.btc.iter().map(|(date, row)| {
            (
                date.as_str(),
                [
                    row.ibit.as_str(),
                    row.fbtc.as_str(),
                    row.bitb.as_str(),
                    row.arkb.as_str(),
                    row.btco.as_str(),
                    row.ezbc.as_str(),
                    row.brrr.as_str(),
                    row.hodl.as_str(),
                    row.btcw.as_str(),
                    row.msbt.as_str(),
                    row.gbtc.as_str(),
                    row.btc.as_str(),
                ],
            )
        }),
    )?;
    let eth = parse_rows(
        CryptocurrencyEtfAsset::Eth,
        &ETH_FUNDS,
        snapshot.eth.iter().map(|(date, row)| {
            (
                date.as_str(),
                [
                    row.etha.as_str(),
                    row.ethb.as_str(),
                    row.feth.as_str(),
                    row.ethw.as_str(),
                    row.teth.as_str(),
                    row.ethv.as_str(),
                    row.qeth.as_str(),
                    row.ezet.as_str(),
                    row.ethe.as_str(),
                    row.eth.as_str(),
                ],
            )
        }),
    )?;
    let sol = parse_rows(
        CryptocurrencyEtfAsset::Sol,
        &SOL_FUNDS,
        snapshot.sol.iter().map(|(date, row)| {
            (
                date.as_str(),
                [
                    row.bsol.as_str(),
                    row.vsol.as_str(),
                    row.fsol.as_str(),
                    row.tsol.as_str(),
                    row.soez.as_str(),
                    row.gsol.as_str(),
                ],
            )
        }),
    )?;
    let hype = parse_rows(
        CryptocurrencyEtfAsset::Hyp,
        &HYPE_FUNDS,
        snapshot.hype.iter().map(|(date, row)| {
            (
                date.as_str(),
                [row.bhyp.as_str(), row.thyp.as_str(), row.hypg.as_str()],
            )
        }),
    )?;

    Ok(CryptocurrencyEtfSnapshot {
        datasets: [Arc::new(btc), Arc::new(eth), Arc::new(sol), Arc::new(hype)],
    })
}

/// Reads the dated rows of one asset into its history.
fn parse_rows<'a, I, const N: usize>(
    asset: CryptocurrencyEtfAsset,
    funds: &[&'static str; N],
    rows: I,
) -> Result<CryptocurrencyEtfDataset, CryptocurrencyEtfParseError>
where
    I: IntoIterator<Item = (&'a str, [&'a str; N])>,
{
    let mut parsed_days = BTreeMap::new();
    for (date_text, values) in rows {
        let date = parse_date(asset, date_text)?;
        let mut fund_flows = Vec::with_capacity(funds.len());
        let mut total_flow = 0.0;
        for (fund, value) in funds.iter().zip(values) {
            let flow =
                parse_number(value).ok_or_else(|| CryptocurrencyEtfParseError::InvalidNumber {
                    asset,
                    date: date_text.into(),
                    fund: (*fund).into(),
                    value: value.into(),
                })?;
            total_flow += flow;
            if !total_flow.is_finite() {
                return Err(CryptocurrencyEtfParseError::NonFiniteTotal { asset, date });
            }
            fund_flows.push(flow);
        }
        // The limit is checked before the insert rather than after, so a response
        // that is too large is refused instead of being built and then measured.
        let at_capacity = parsed_days.len() == MAX_ETF_DAYS_PER_ASSET;
        match parsed_days.entry(date) {
            Entry::Occupied(_) => {
                return Err(CryptocurrencyEtfParseError::DuplicateDate { asset, date });
            }
            Entry::Vacant(entry) => {
                if at_capacity {
                    return Err(CryptocurrencyEtfParseError::TooManyRows {
                        asset,
                        maximum: MAX_ETF_DAYS_PER_ASSET,
                    });
                }
                entry.insert(ParsedDay {
                    fund_flows: fund_flows.into(),
                    total_flow,
                });
            }
        }
    }
    if parsed_days.is_empty() {
        return Err(CryptocurrencyEtfParseError::EmptyAssetMap { asset });
    }

    let mut net_assets = 0.0;
    let mut days = Vec::with_capacity(parsed_days.len());
    for (date, parsed) in parsed_days {
        net_assets += parsed.total_flow;
        if !net_assets.is_finite() {
            return Err(CryptocurrencyEtfParseError::NonFiniteAssets { asset, date });
        }
        days.push(CryptocurrencyEtfDay {
            date,
            fund_flows: parsed.fund_flows,
            total_flow: parsed.total_flow,
            net_assets,
        });
    }

    Ok(CryptocurrencyEtfDataset {
        asset,
        funds: Arc::from((*funds).map(SmolStr::new_static)),
        days: days.into(),
    })
}

/// Reads an ISO trading date, refusing anything a `Date` would have to guess at.
fn parse_date(
    asset: CryptocurrencyEtfAsset,
    value: &str,
) -> Result<Date, CryptocurrencyEtfParseError> {
    let invalid_date = || CryptocurrencyEtfParseError::InvalidDate {
        asset,
        value: value.into(),
    };
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4]
            .iter()
            .chain(&bytes[5..7])
            .chain(&bytes[8..])
            .all(u8::is_ascii_digit)
    {
        return Err(invalid_date());
    }

    let parse =
        |start: usize, end: usize| value[start..end].parse::<u16>().map_err(|_| invalid_date());
    let year = i32::from(parse(0, 4)?);
    let month = Month::try_from(u8::try_from(parse(5, 7)?).map_err(|_| invalid_date())?)
        .map_err(|_| invalid_date())?;
    let day = u8::try_from(parse(8, 10)?).map_err(|_| invalid_date())?;
    Date::from_calendar_date(year, month, day).map_err(|_| invalid_date())
}

/// Reads one cell of a row.
///
/// The upstream sheet is a spreadsheet export, so a cell is empty, a dash, a
/// number with thousands separators, or a number in accounting parentheses. All
/// four are readings; anything else is not, and `NaN` in particular must not
/// become a point the chart would draw.
fn parse_number(value: &str) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() || value == "-" {
        return Some(0.0);
    }
    let (negative, value) = if let Some(value) = value.strip_prefix('(') {
        let value = value.strip_suffix(')')?;
        (true, value.trim())
    } else {
        if value.contains('(') || value.contains(')') {
            return None;
        }
        (false, value)
    };
    if value.is_empty() || value == "-" {
        return None;
    }
    let value = value.replace(',', "");
    let parsed = value.parse::<f64>().ok()?;
    if !parsed.is_finite() {
        return None;
    }
    Some(if negative { -parsed } else { parsed })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use kytrix_proto::types::cryptocurrency_etf;

    use super::*;

    fn date(year: i32, month: Month, day: u8) -> Date {
        Date::from_calendar_date(year, month, day).expect("fixture date should be valid")
    }

    fn valid_snapshot() -> proto_types::CryptocurrencyEtf {
        let mut btc = HashMap::new();
        btc.insert(
            "2024-01-03".into(),
            cryptocurrency_etf::Btc {
                ibit: "(50.0)".into(),
                fbtc: "-".into(),
                ..Default::default()
            },
        );
        btc.insert(
            "2024-01-02".into(),
            cryptocurrency_etf::Btc {
                ibit: "1,200.5".into(),
                fbtc: "-200.5".into(),
                ..Default::default()
            },
        );
        proto_types::CryptocurrencyEtf {
            btc,
            eth: HashMap::from([("2024-01-04".into(), cryptocurrency_etf::Eth::default())]),
            sol: HashMap::from([("2024-01-05".into(), cryptocurrency_etf::Sol::default())]),
            hype: HashMap::from([("2024-01-06".into(), cryptocurrency_etf::Hype::default())]),
            timestamp: 0,
        }
    }

    #[test]
    fn parse_should_use_fixed_ticker_order_and_sorted_dates() {
        let snapshot = parse_etf(valid_snapshot()).expect("valid protobuf snapshot should parse");
        let btc = snapshot.dataset(CryptocurrencyEtfAsset::Btc);

        assert_eq!(
            btc.funds.as_ref(),
            BTC_FUNDS.map(SmolStr::new_static).as_slice()
        );
        assert_eq!(btc.days[0].date, date(2024, Month::January, 2));
        assert_eq!(btc.days[0].fund_flows[0], 1_200.5);
        assert_eq!(btc.days[0].fund_flows[1], -200.5);
        assert_eq!(btc.days[0].total_flow, 1_000.0);
        assert_eq!(btc.days[0].net_assets, 1_000.0);
        assert_eq!(btc.days[1].date, date(2024, Month::January, 3));
        assert_eq!(btc.days[1].net_assets, 950.0);
        assert_eq!(
            snapshot.dataset(CryptocurrencyEtfAsset::Eth).funds[0],
            "ETHA"
        );
        assert_eq!(
            snapshot.dataset(CryptocurrencyEtfAsset::Sol).funds[0],
            "BSOL"
        );
        assert_eq!(
            snapshot.dataset(CryptocurrencyEtfAsset::Hyp).funds[0],
            "BHYP"
        );
    }

    #[test]
    fn parse_should_accept_empty_dash_thousands_and_parenthesized_values() {
        assert_eq!(parse_number(""), Some(0.0));
        assert_eq!(parse_number("-"), Some(0.0));
        assert_eq!(parse_number("1,234.5"), Some(1_234.5));
        assert_eq!(parse_number("(1,234.5)"), Some(-1_234.5));
    }

    #[test]
    fn parse_should_reject_invalid_dates_numbers_and_empty_maps() {
        let mut snapshot = valid_snapshot();
        snapshot.btc = HashMap::from([("2024-02-30".into(), cryptocurrency_etf::Btc::default())]);
        assert!(matches!(
            parse_etf(snapshot.clone()),
            Err(CryptocurrencyEtfParseError::InvalidDate {
                asset: CryptocurrencyEtfAsset::Btc,
                ..
            })
        ));

        snapshot.btc = HashMap::from([(
            "2024-01-01".into(),
            cryptocurrency_etf::Btc {
                ibit: "NaN".into(),
                ..Default::default()
            },
        )]);
        assert!(matches!(
            parse_etf(snapshot.clone()),
            Err(CryptocurrencyEtfParseError::InvalidNumber {
                asset: CryptocurrencyEtfAsset::Btc,
                ..
            })
        ));

        snapshot.btc.clear();
        assert!(matches!(
            parse_etf(snapshot),
            Err(CryptocurrencyEtfParseError::EmptyAssetMap {
                asset: CryptocurrencyEtfAsset::Btc
            })
        ));
    }

    #[test]
    fn parse_should_reject_non_finite_totals_and_assets() {
        let mut snapshot = valid_snapshot();
        snapshot.btc = HashMap::from([(
            "2024-01-01".into(),
            cryptocurrency_etf::Btc {
                ibit: "1e308".into(),
                fbtc: "1e308".into(),
                ..Default::default()
            },
        )]);
        assert!(matches!(
            parse_etf(snapshot.clone()),
            Err(CryptocurrencyEtfParseError::NonFiniteTotal {
                asset: CryptocurrencyEtfAsset::Btc,
                ..
            })
        ));

        snapshot.btc = HashMap::from([
            (
                "2024-01-01".into(),
                cryptocurrency_etf::Btc {
                    ibit: "1e308".into(),
                    ..Default::default()
                },
            ),
            (
                "2024-01-02".into(),
                cryptocurrency_etf::Btc {
                    ibit: "1e308".into(),
                    ..Default::default()
                },
            ),
        ]);
        assert!(matches!(
            parse_etf(snapshot),
            Err(CryptocurrencyEtfParseError::NonFiniteAssets {
                asset: CryptocurrencyEtfAsset::Btc,
                ..
            })
        ));
    }

    #[test]
    fn parse_rows_should_enforce_the_per_asset_history_limit() {
        let dates = |count: usize| {
            (0..count)
                .map(|offset| {
                    Date::from_julian_day(2_451_545 + offset as i32)
                        .expect("fixture date should be valid")
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        let accepted_dates = dates(MAX_ETF_DAYS_PER_ASSET);
        let accepted = parse_rows(
            CryptocurrencyEtfAsset::Hyp,
            &HYPE_FUNDS,
            accepted_dates
                .iter()
                .map(|date| (date.as_str(), ["0"; HYPE_FUNDS.len()])),
        )
        .expect("the configured history limit should be accepted");
        assert_eq!(accepted.days.len(), MAX_ETF_DAYS_PER_ASSET);

        let oversized_dates = dates(MAX_ETF_DAYS_PER_ASSET + 1);
        assert!(matches!(
            parse_rows(
                CryptocurrencyEtfAsset::Hyp,
                &HYPE_FUNDS,
                oversized_dates
                    .iter()
                    .map(|date| (date.as_str(), ["0"; HYPE_FUNDS.len()])),
            ),
            Err(CryptocurrencyEtfParseError::TooManyRows {
                asset: CryptocurrencyEtfAsset::Hyp,
                maximum: MAX_ETF_DAYS_PER_ASSET,
            })
        ));
    }

    #[test]
    fn days_for_window_should_take_latest_rows_without_allocating() {
        let days = (1..=35)
            .map(|ordinal| CryptocurrencyEtfDay {
                date: Date::from_ordinal_date(2024, ordinal).expect("test ordinal should be valid"),
                fund_flows: Arc::from([f64::from(ordinal)]),
                total_flow: f64::from(ordinal),
                net_assets: f64::from(ordinal),
            })
            .collect::<Vec<_>>();
        let dataset = CryptocurrencyEtfDataset {
            asset: CryptocurrencyEtfAsset::Btc,
            funds: Arc::from([SmolStr::new_static("IBIT")]),
            days: days.into(),
        };

        let window = dataset.days_for_window(CryptocurrencyEtfWindow::Days30);
        assert_eq!(
            (window.len(), window.first().map(|day| day.date.ordinal())),
            (30, Some(6))
        );
    }

    #[test]
    fn a_named_fund_the_asset_does_not_have_should_have_no_flow() {
        let dataset = CryptocurrencyEtfDataset {
            asset: CryptocurrencyEtfAsset::Btc,
            funds: Arc::from([SmolStr::new_static("IBIT")]),
            days: Arc::from([CryptocurrencyEtfDay {
                date: date(2024, Month::January, 2),
                fund_flows: Arc::from([3.0]),
                total_flow: 3.0,
                net_assets: 3.0,
            }]),
        };
        let day = &dataset.days[0];

        assert_eq!(
            dataset.flow_for(day, &CryptocurrencyEtfFundSelection::All),
            Some(3.0)
        );
        assert_eq!(
            dataset.flow_for(
                day,
                &CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("IBIT"))
            ),
            Some(3.0)
        );
        assert_eq!(
            dataset.flow_for(
                day,
                &CryptocurrencyEtfFundSelection::Named(SmolStr::new_static("FBTC"))
            ),
            None
        );
    }
}
