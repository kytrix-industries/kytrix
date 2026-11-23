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

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Adjustment {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "forward")]
    Forward,
}

impl Adjustment {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Forward => "forward",
        }
    }
}

impl Default for Adjustment {
    fn default() -> Self {
        Self::None
    }
}

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
