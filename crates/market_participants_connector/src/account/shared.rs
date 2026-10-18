//! Private value objects shared by the market-participants capabilities.
//!
//! These types are owned by the capability boundary. Protocol DTOs may use them
//! internally, but they are not the public model: an order quantity, a price and
//! a quantity are `market_participants_model` values, and this module never defines a second
//! unit vocabulary of its own.

use std::{fmt, num::NonZeroU8, str::FromStr};

use market_participants_model::{
    decimal::{Price, UnixMillis},
    error::ModelError,
};
use serde::{Deserialize, Serialize, de};
use smol_str::SmolStr;

macro_rules! identifier {
    ($name:ident, $kind:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(SmolStr);

        impl $name {
            pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
                let value = value.as_ref().trim();
                if value.is_empty() {
                    Err(ModelError::EmptyIdentifier { field: $kind })
                } else {
                    Ok(Self(SmolStr::new(value)))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ModelError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = SmolStr::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

identifier!(PositionId, "position ID");
identifier!(AccountTradeId, "account trade ID");

/// API namespace used to address a market-participant order.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum OrderRoute {
    #[default]
    Normal,
    Algo,
}

/// Market-participant order identity including its API namespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OrderId {
    value: SmolStr,
    route: OrderRoute,
}

impl OrderId {
    pub fn new(value: impl AsRef<str>, route: OrderRoute) -> Result<Self, ModelError> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(ModelError::EmptyIdentifier { field: "order ID" });
        }
        Ok(Self {
            value: SmolStr::new(value),
            route,
        })
    }

    pub fn normal(value: impl AsRef<str>) -> Result<Self, ModelError> {
        Self::new(value, OrderRoute::Normal)
    }

    pub fn algo(value: impl AsRef<str>) -> Result<Self, ModelError> {
        Self::new(value, OrderRoute::Algo)
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub const fn route(&self) -> OrderRoute {
        self.route
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OrderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct OrderIdWire {
            value: SmolStr,
            route: OrderRoute,
        }

        let wire = OrderIdWire::deserialize(deserializer)?;
        Self::new(wire.value, wire.route).map_err(de::Error::custom)
    }
}

/// Margin accounting mode for a perpetual position or order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarginMode {
    Isolated,
    Cross,
}

/// Account-wide perpetual position mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PositionMode {
    #[default]
    OneWay,
    Hedge,
}

/// Price source used to trigger a conditional order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TriggerPriceKind {
    Last,
    Mark,
    Index,
}

/// Current lifecycle state of an order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    Expired,
}

/// Market-participant time-in-force instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimeInForce {
    GoodTillCancelled,
    ImmediateOrCancel,
    FillOrKill,
    PostOnly,
    GoodTillDate(UnixMillis),
}

/// The direction Hyperliquid reports for one fill.
///
/// The venue's own spelling is the wire value; it is not a side, because it also
/// distinguishes opening from closing an existing position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HyperliquidFillDirection {
    #[serde(rename = "Open Long")]
    OpenLong,
    #[serde(rename = "Close Long")]
    CloseLong,
    #[serde(rename = "Open Short")]
    OpenShort,
    #[serde(rename = "Close Short")]
    CloseShort,
    #[serde(rename = "Long > Short")]
    LongToShort,
    #[serde(rename = "Short > Long")]
    ShortToLong,
    #[serde(rename = "Liquidated Cross Long")]
    LiquidatedCrossLong,
    #[serde(rename = "Liquidated Cross Short")]
    LiquidatedCrossShort,
    #[serde(rename = "Liquidated Isolated Long")]
    LiquidatedIsolatedLong,
    #[serde(rename = "Liquidated Isolated Short")]
    LiquidatedIsolatedShort,
    #[serde(rename = "Auto-Deleveraging")]
    AutoDeleveraging,
    #[serde(rename = "Partial Borrow Liquidation")]
    PartialBorrowLiquidation,
    #[serde(rename = "Backstop Borrow Liquidation")]
    BackstopBorrowLiquidation,
    #[serde(rename = "Settlement")]
    Settlement,
    #[serde(rename = "Net Child Vaults")]
    NetChildVaults,
    #[serde(rename = "Buy")]
    Buy,
    #[serde(rename = "Sell")]
    Sell,
    #[serde(rename = "Spot Dust Conversion")]
    SpotDustConversion,
}

impl HyperliquidFillDirection {
    /// Decodes the venue's own spelling.
    ///
    /// Every spelling the venue documents is covered, including the flips
    /// (`Long > Short` and `Short > Long`) and the liquidation and
    /// auto-deleveraging events: one of them reaching the decoder must not fail
    /// the whole reading it arrived in.
    pub fn from_raw(value: &str) -> Result<Self, SmolStr> {
        match value {
            "Open Long" => Ok(Self::OpenLong),
            "Close Long" => Ok(Self::CloseLong),
            "Open Short" => Ok(Self::OpenShort),
            "Close Short" => Ok(Self::CloseShort),
            "Long > Short" => Ok(Self::LongToShort),
            "Short > Long" => Ok(Self::ShortToLong),
            "Liquidated Cross Long" => Ok(Self::LiquidatedCrossLong),
            "Liquidated Cross Short" => Ok(Self::LiquidatedCrossShort),
            "Liquidated Isolated Long" => Ok(Self::LiquidatedIsolatedLong),
            "Liquidated Isolated Short" => Ok(Self::LiquidatedIsolatedShort),
            "Auto-Deleveraging" => Ok(Self::AutoDeleveraging),
            "Partial Borrow Liquidation" => Ok(Self::PartialBorrowLiquidation),
            "Backstop Borrow Liquidation" => Ok(Self::BackstopBorrowLiquidation),
            "Settlement" => Ok(Self::Settlement),
            "Net Child Vaults" => Ok(Self::NetChildVaults),
            "Buy" => Ok(Self::Buy),
            "Sell" => Ok(Self::Sell),
            "Spot Dust Conversion" => Ok(Self::SpotDustConversion),
            other => Err(SmolStr::new(other)),
        }
    }

    /// Returns the venue's own spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenLong => "Open Long",
            Self::CloseLong => "Close Long",
            Self::OpenShort => "Open Short",
            Self::CloseShort => "Close Short",
            Self::LongToShort => "Long > Short",
            Self::ShortToLong => "Short > Long",
            Self::LiquidatedCrossLong => "Liquidated Cross Long",
            Self::LiquidatedCrossShort => "Liquidated Cross Short",
            Self::LiquidatedIsolatedLong => "Liquidated Isolated Long",
            Self::LiquidatedIsolatedShort => "Liquidated Isolated Short",
            Self::AutoDeleveraging => "Auto-Deleveraging",
            Self::PartialBorrowLiquidation => "Partial Borrow Liquidation",
            Self::BackstopBorrowLiquidation => "Backstop Borrow Liquidation",
            Self::Settlement => "Settlement",
            Self::NetChildVaults => "Net Child Vaults",
            Self::Buy => "Buy",
            Self::Sell => "Sell",
            Self::SpotDustConversion => "Spot Dust Conversion",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HyperliquidFillDirection;

    /// Every wire spelling the venue documents decodes and round-trips.
    #[test]
    fn every_documented_fill_direction_round_trips() {
        for value in [
            "Open Long",
            "Close Long",
            "Open Short",
            "Close Short",
            "Long > Short",
            "Short > Long",
            "Liquidated Cross Long",
            "Liquidated Cross Short",
            "Liquidated Isolated Long",
            "Liquidated Isolated Short",
            "Auto-Deleveraging",
            "Partial Borrow Liquidation",
            "Backstop Borrow Liquidation",
            "Settlement",
            "Net Child Vaults",
            "Buy",
            "Sell",
            "Spot Dust Conversion",
        ] {
            let direction =
                HyperliquidFillDirection::from_raw(value).unwrap_or_else(|_| panic!("{value}"));
            assert_eq!(direction.as_str(), value);
        }
    }

    /// A spelling the venue has not published is still reported rather than guessed.
    #[test]
    fn an_unknown_fill_direction_is_reported() {
        assert_eq!(
            HyperliquidFillDirection::from_raw("Something New"),
            Err("Something New".into())
        );
    }
}

/// Fields whose validity depends on the order category.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum OrderKind {
    #[serde(rename = "market")]
    Market,
    #[serde(rename = "limit")]
    Limit {
        price: Price,
        time_in_force: TimeInForce,
    },
    #[serde(rename = "stop_market")]
    StopMarket {
        trigger_price: Price,
        trigger_price_kind: TriggerPriceKind,
    },
    #[serde(rename = "stop_limit")]
    StopLimit {
        price: Price,
        trigger_price: Price,
        trigger_price_kind: TriggerPriceKind,
        time_in_force: TimeInForce,
    },
    #[serde(rename = "take_profit_market")]
    TakeProfitMarket {
        trigger_price: Price,
        trigger_price_kind: TriggerPriceKind,
    },
    #[serde(rename = "take_profit_limit")]
    TakeProfitLimit {
        price: Price,
        trigger_price: Price,
        trigger_price_kind: TriggerPriceKind,
        time_in_force: TimeInForce,
    },
}

/// Direction of a non-negative perpetual position quantity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PositionDirection {
    Long,
    Short,
}

/// Whether an account trade supplied or consumed resting liquidity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LiquidityRole {
    Maker,
    Taker,
}

/// Authenticated account settings relevant to one perpetual listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradingSettings {
    pub position_mode: PositionMode,
    pub margin_mode: MarginMode,
    pub leverage: NonZeroU8,
}

/// Final state of a closed perpetual position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HistoricalPositionStatus {
    PartiallyClosed,
    Closed,
    Liquidated,
    PartiallyLiquidated,
    AutoDeleveraged,
}
