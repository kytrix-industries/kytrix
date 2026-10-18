//! The intervals and price-adjustment conventions a market request asks one venue
//! for.
//!
//! A candlestick request and a perpetual-statistics request are both chosen by
//! their interval, and a candlestick request additionally names how its prices are
//! adjusted, so all three closed sets are part of the market facade a caller builds
//! its request from. Which intervals and which adjustment conventions a route
//! publishes is its own capability descriptor's answer; these enums are what the
//! model can express, so a request can never carry a spelling instead of a value.

/// One candlestick interval, as the venues of this crate name them.
///
/// Which intervals a route publishes is decided by its own capability
/// descriptor; this enum is the closed set of intervals the model can express,
/// so a query can never carry a spelling instead of an interval.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum TimeFrame {
    #[serde(rename = "1s")]
    Second1,
    #[serde(rename = "1m")]
    Minute1,
    #[serde(rename = "2m")]
    Minute2,
    #[serde(rename = "3m")]
    Minute3,
    #[serde(rename = "5m")]
    Minute5,
    #[serde(rename = "10m")]
    Minute10,
    #[serde(rename = "15m")]
    Minute15,
    #[serde(rename = "20m")]
    Minute20,
    #[serde(rename = "30m")]
    Minute30,
    #[serde(rename = "45m")]
    Minute45,
    #[serde(rename = "1h")]
    Hour1,
    #[serde(rename = "2h")]
    Hour2,
    #[serde(rename = "3h")]
    Hour3,
    #[serde(rename = "4h")]
    Hour4,
    #[serde(rename = "6h")]
    Hour6,
    #[serde(rename = "8h")]
    Hour8,
    #[serde(rename = "12h")]
    Hour12,
    #[serde(rename = "1d")]
    Day1,
    #[serde(rename = "2d")]
    Day2,
    #[serde(rename = "3d")]
    Day3,
    #[serde(rename = "1w")]
    Week1,
    #[serde(rename = "1M")]
    Month1,
    #[serde(rename = "3M")]
    Month3,
    #[serde(rename = "1Q")]
    Quarter1,
    #[serde(rename = "1Y")]
    Year1,
}

impl TimeFrame {
    /// Returns the canonical label of this interval.
    ///
    /// This is the model's own spelling, not a venue's: each venue encodes its
    /// wire form in its own codec, and this label is what names the interval in
    /// an error or a log.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Second1 => "1s",
            Self::Minute1 => "1m",
            Self::Minute2 => "2m",
            Self::Minute3 => "3m",
            Self::Minute5 => "5m",
            Self::Minute10 => "10m",
            Self::Minute15 => "15m",
            Self::Minute20 => "20m",
            Self::Minute30 => "30m",
            Self::Minute45 => "45m",
            Self::Hour1 => "1h",
            Self::Hour2 => "2h",
            Self::Hour3 => "3h",
            Self::Hour4 => "4h",
            Self::Hour6 => "6h",
            Self::Hour8 => "8h",
            Self::Hour12 => "12h",
            Self::Day1 => "1d",
            Self::Day2 => "2d",
            Self::Day3 => "3d",
            Self::Week1 => "1w",
            Self::Month1 => "1M",
            Self::Month3 => "3M",
            Self::Quarter1 => "1Q",
            Self::Year1 => "1Y",
        }
    }
}

/// One price-adjustment convention a candlestick request can carry.
///
/// A venue that publishes no adjusted history offers exactly one value, `None`,
/// and a venue whose history is published both raw and adjusted offers both. The
/// set a route publishes is its own capability descriptor's answer.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Adjustment {
    /// Prices exactly as the venue published them at the time of the trade.
    #[serde(rename = "none")]
    None,
    /// Prices restated so that a split or a dividend in the window does not open
    /// a gap: the history before the event is scaled to the latest basis.
    #[serde(rename = "forward")]
    Forward,
}

impl Adjustment {
    /// Returns the canonical label of this adjustment.
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Forward => "forward",
        }
    }
}

impl Default for Adjustment {
    /// `None` 是「不改变价格语义」的中性取值，对任何 venue 都成立，因此它可以诚实地做默认值。
    fn default() -> Self {
        Self::None
    }
}

/// The time unit of one perpetual-statistics query.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum StatisticsTimeFrame {
    #[serde(rename = "5m")]
    Minute5,
    #[serde(rename = "15m")]
    Minute15,
    #[serde(rename = "30m")]
    Minute30,
    #[serde(rename = "1h")]
    Hour1,
    #[serde(rename = "2h")]
    Hour2,
    #[serde(rename = "4h")]
    Hour4,
    #[serde(rename = "6h")]
    Hour6,
    #[serde(rename = "12h")]
    Hour12,
    #[serde(rename = "1d")]
    Day1,
}

impl StatisticsTimeFrame {
    /// Returns the canonical label of this statistics interval.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Minute5 => "5m",
            Self::Minute15 => "15m",
            Self::Minute30 => "30m",
            Self::Hour1 => "1h",
            Self::Hour2 => "2h",
            Self::Hour4 => "4h",
            Self::Hour6 => "6h",
            Self::Hour12 => "12h",
            Self::Day1 => "1d",
        }
    }
}
