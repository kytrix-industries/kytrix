//! Venue trading rules: price, quantity, session, margin, funding and
//! per-product rule families.

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{
    asset::AssetId,
    decimal::{Fraction, Price, Quantity, Tick, UnixMillis},
    error::{ModelError, require_non_negative_time, require_positive_quantity},
    identifier::TradingCalendarId,
    instrument::{Instrument, InstrumentKind},
    listing::venue::PriceBasis,
    market::units::QuantityUnit,
};

/// The full rule set of one listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TradingRules {
    #[serde(rename = "common")]
    pub common: CommonTradingRules,
    #[serde(rename = "product")]
    pub product: ProductTradingRules,
}

impl TradingRules {
    /// Validates the common rules, the product rules and their agreement.
    pub fn validate(&self) -> Result<(), ModelError> {
        self.common.validate()?;
        self.product.validate()
    }

    /// Validates these rules against the instrument the listing implements.
    pub fn validate_for(&self, instrument: &Instrument) -> Result<(), ModelError> {
        self.validate()?;
        if self.product.instrument_kind() != instrument.kind() {
            return Err(ModelError::TradingRulesMismatch {
                field: "product rules do not match the instrument kind",
            });
        }
        let (quote_asset, basis) = crate::listing::navigation::expected_price_terms(instrument)?;
        if self.common.price.quote_asset != quote_asset {
            return Err(ModelError::TradingRulesMismatch {
                field: "price.quote_asset",
            });
        }
        if self.common.price.basis != basis {
            return Err(ModelError::TradingRulesMismatch {
                field: "price.basis",
            });
        }
        if let Some(notional) = &self.common.notional
            && notional.asset != quote_asset
        {
            return Err(ModelError::TradingRulesMismatch {
                field: "notional.asset",
            });
        }
        self.common
            .quantity
            .unit
            .validate_for_instrument(instrument)?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct TradingRulesWire {
    #[serde(rename = "common")]
    common: CommonTradingRules,
    #[serde(rename = "product")]
    product: ProductTradingRules,
}

impl<'de> Deserialize<'de> for TradingRules {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TradingRulesWire::deserialize(deserializer)?;
        let rules = Self {
            common: wire.common,
            product: wire.product,
        };
        rules.validate().map_err(de::Error::custom)?;
        Ok(rules)
    }
}

/// The increments one listing publishes its prices and quantities in.
///
/// A number is read at the precision of the listing it belongs to: `90000` at a
/// price tick of `0.01` is the price `90000.00`, not the integer it would be
/// rendered as by the canonical [`Display`](std::fmt::Display) of `Price`, which
/// drops trailing fractional zeroes by contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListingPrecision {
    /// The increment every price of the listing is a multiple of.
    pub price_tick: Tick,
    /// The increment every traded quantity of the listing is a multiple of.
    pub quantity_tick: Tick,
}

impl ListingPrecision {
    /// Reads the increments of one rule set.
    ///
    /// A price tick is non-zero by its own type. A quantity step is a canonical
    /// quantity, so a rule set that carries none — or carries zero — leaves
    /// quantities written at whole units rather than refusing to read them.
    pub fn of(rules: &TradingRules) -> Self {
        Self {
            price_tick: rules.common.price.tick,
            quantity_tick: Tick::try_from(rules.common.quantity.step).unwrap_or_else(|_| {
                Tick::try_from(Quantity::from(1_u8)).expect("a whole unit is a valid tick")
            }),
        }
    }
}

/// The rules that apply to every product family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommonTradingRules {
    #[serde(rename = "price")]
    pub price: PriceRule,
    #[serde(rename = "quantity")]
    pub quantity: QuantityRule,
    #[serde(rename = "notional")]
    pub notional: Option<NotionalRule>,
    #[serde(rename = "sessions")]
    pub sessions: TradingCalendarId,
    #[serde(rename = "settlement_cycle")]
    pub settlement_cycle: SettlementCycle,
    #[serde(rename = "price_limits")]
    pub price_limits: PriceLimitRule,
}

impl CommonTradingRules {
    /// Validates the price, quantity, notional and price-limit constraints.
    pub fn validate(&self) -> Result<(), ModelError> {
        self.price.validate()?;
        self.quantity.validate()?;
        if let Some(notional) = &self.notional {
            notional.validate()?;
        }
        self.price_limits.validate()
    }
}

#[derive(Deserialize)]
struct CommonTradingRulesWire {
    #[serde(rename = "price")]
    price: PriceRule,
    #[serde(rename = "quantity")]
    quantity: QuantityRule,
    #[serde(rename = "notional")]
    notional: Option<NotionalRule>,
    #[serde(rename = "sessions")]
    sessions: TradingCalendarId,
    #[serde(rename = "settlement_cycle")]
    settlement_cycle: SettlementCycle,
    #[serde(rename = "price_limits")]
    price_limits: PriceLimitRule,
}

impl<'de> Deserialize<'de> for CommonTradingRules {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CommonTradingRulesWire::deserialize(deserializer)?;
        let rules = Self {
            price: wire.price,
            quantity: wire.quantity,
            notional: wire.notional,
            sessions: wire.sessions,
            settlement_cycle: wire.settlement_cycle,
            price_limits: wire.price_limits,
        };
        rules.validate().map_err(de::Error::custom)?;
        Ok(rules)
    }
}

/// The price constraints of a listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PriceRule {
    #[serde(rename = "quote_asset")]
    pub quote_asset: AssetId,
    #[serde(rename = "basis")]
    pub basis: PriceBasis,
    #[serde(rename = "tick")]
    pub tick: Tick,
    #[serde(rename = "minimum")]
    pub minimum: Option<Price>,
    #[serde(rename = "maximum")]
    pub maximum: Option<Price>,
}

impl PriceRule {
    /// Validates the price rule.
    ///
    /// The tick precision is already constrained by [`Tick`] itself, which can
    /// never be zero, so this only checks the published bounds.
    pub fn validate(&self) -> Result<(), ModelError> {
        if let (Some(minimum), Some(maximum)) = (self.minimum, self.maximum)
            && minimum > maximum
        {
            return Err(ModelError::InvalidTimeRange);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct PriceRuleWire {
    #[serde(rename = "quote_asset")]
    quote_asset: AssetId,
    #[serde(rename = "basis")]
    basis: PriceBasis,
    #[serde(rename = "tick")]
    tick: Tick,
    #[serde(rename = "minimum")]
    minimum: Option<Price>,
    #[serde(rename = "maximum")]
    maximum: Option<Price>,
}

impl<'de> Deserialize<'de> for PriceRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PriceRuleWire::deserialize(deserializer)?;
        let rule = Self {
            quote_asset: wire.quote_asset,
            basis: wire.basis,
            tick: wire.tick,
            minimum: wire.minimum,
            maximum: wire.maximum,
        };
        rule.validate().map_err(de::Error::custom)?;
        Ok(rule)
    }
}

/// The quantity constraints of a listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QuantityRule {
    #[serde(rename = "unit")]
    pub unit: QuantityUnit,
    #[serde(rename = "step")]
    pub step: Quantity,
    #[serde(rename = "minimum")]
    pub minimum: Option<Quantity>,
    #[serde(rename = "maximum")]
    pub maximum: Option<Quantity>,
}

impl QuantityRule {
    /// Validates the quantity rule.
    pub fn validate(&self) -> Result<(), ModelError> {
        require_positive_quantity(self.step, "quantity.step")?;
        if let (Some(minimum), Some(maximum)) = (self.minimum, self.maximum)
            && minimum > maximum
        {
            return Err(ModelError::InvalidTimeRange);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct QuantityRuleWire {
    #[serde(rename = "unit")]
    unit: QuantityUnit,
    #[serde(rename = "step")]
    step: Quantity,
    #[serde(rename = "minimum")]
    minimum: Option<Quantity>,
    #[serde(rename = "maximum")]
    maximum: Option<Quantity>,
}

impl<'de> Deserialize<'de> for QuantityRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = QuantityRuleWire::deserialize(deserializer)?;
        let rule = Self {
            unit: wire.unit,
            step: wire.step,
            minimum: wire.minimum,
            maximum: wire.maximum,
        };
        rule.validate().map_err(de::Error::custom)?;
        Ok(rule)
    }
}

/// The monetary minimum and maximum of one listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NotionalRule {
    #[serde(rename = "asset")]
    pub asset: AssetId,
    #[serde(rename = "minimum")]
    pub minimum: Option<Quantity>,
    #[serde(rename = "maximum")]
    pub maximum: Option<Quantity>,
}

impl NotionalRule {
    /// Validates the notional bounds.
    pub fn validate(&self) -> Result<(), ModelError> {
        if let (Some(minimum), Some(maximum)) = (self.minimum, self.maximum)
            && minimum > maximum
        {
            return Err(ModelError::InvalidTimeRange);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct NotionalRuleWire {
    #[serde(rename = "asset")]
    asset: AssetId,
    #[serde(rename = "minimum")]
    minimum: Option<Quantity>,
    #[serde(rename = "maximum")]
    maximum: Option<Quantity>,
}

impl<'de> Deserialize<'de> for NotionalRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = NotionalRuleWire::deserialize(deserializer)?;
        let rule = Self {
            asset: wire.asset,
            minimum: wire.minimum,
            maximum: wire.maximum,
        };
        rule.validate().map_err(de::Error::custom)?;
        Ok(rule)
    }
}

/// When a trade settles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "cycle")]
pub enum SettlementCycle {
    #[serde(rename = "immediate")]
    Immediate,
    #[serde(rename = "t_plus")]
    TPlus {
        #[serde(rename = "business_days")]
        business_days: u8,
    },
    #[serde(rename = "at_expiry")]
    AtExpiry,
    #[serde(rename = "on_chain")]
    OnChain {
        #[serde(rename = "confirmations")]
        confirmations: u32,
    },
}

impl SettlementCycle {
    /// Returns the fixed canonical label of this cycle.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::TPlus { .. } => "t-plus",
            Self::AtExpiry => "at-expiry",
            Self::OnChain { .. } => "on-chain",
        }
    }
}

/// The price limit of one listing session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "limits")]
pub enum PriceLimitRule {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "absolute")]
    Absolute {
        #[serde(rename = "lower")]
        lower: Option<Price>,
        #[serde(rename = "upper")]
        upper: Option<Price>,
    },
    #[serde(rename = "relative")]
    Relative {
        #[serde(rename = "down")]
        down: Fraction,
        #[serde(rename = "up")]
        up: Fraction,
    },
}

impl PriceLimitRule {
    /// Validates the absolute range.
    pub fn validate(&self) -> Result<(), ModelError> {
        if let Self::Absolute {
            lower: Some(lower),
            upper: Some(upper),
        } = self
            && lower > upper
        {
            return Err(ModelError::InvalidTimeRange);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "limits")]
enum PriceLimitRuleWire {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "absolute")]
    Absolute {
        #[serde(rename = "lower")]
        lower: Option<Price>,
        #[serde(rename = "upper")]
        upper: Option<Price>,
    },
    #[serde(rename = "relative")]
    Relative {
        #[serde(rename = "down")]
        down: Fraction,
        #[serde(rename = "up")]
        up: Fraction,
    },
}

impl From<PriceLimitRuleWire> for PriceLimitRule {
    fn from(wire: PriceLimitRuleWire) -> Self {
        match wire {
            PriceLimitRuleWire::None => Self::None,
            PriceLimitRuleWire::Absolute { lower, upper } => Self::Absolute { lower, upper },
            PriceLimitRuleWire::Relative { down, up } => Self::Relative { down, up },
        }
    }
}

impl<'de> Deserialize<'de> for PriceLimitRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let rule = Self::from(PriceLimitRuleWire::deserialize(deserializer)?);
        rule.validate().map_err(de::Error::custom)?;
        Ok(rule)
    }
}

/// A half-open millisecond window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimeWindow {
    #[serde(rename = "start")]
    start: UnixMillis,
    #[serde(rename = "end")]
    end: UnixMillis,
}

impl TimeWindow {
    /// Builds a validated window.
    pub fn new(start: UnixMillis, end: UnixMillis) -> Result<Self, ModelError> {
        require_non_negative_time(start, "time_window.start")?;
        require_non_negative_time(end, "time_window.end")?;
        if start > end {
            return Err(ModelError::InvalidTimeRange);
        }
        Ok(Self { start, end })
    }

    /// Returns the inclusive lower bound.
    pub const fn start(self) -> UnixMillis {
        self.start
    }

    /// Returns the exclusive upper bound.
    pub const fn end(self) -> UnixMillis {
        self.end
    }

    /// Returns the window length in milliseconds.
    pub const fn duration_millis(self) -> UnixMillis {
        self.end - self.start
    }

    /// Returns `true` when `instant` is inside the half-open window.
    pub const fn contains(self, instant: UnixMillis) -> bool {
        instant >= self.start && instant < self.end
    }
}

/// How a spot listing accepts order sizes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "policy")]
pub enum LotPolicy {
    #[serde(rename = "whole_lots")]
    WholeLots {
        #[serde(rename = "lot_size")]
        lot_size: Quantity,
    },
    #[serde(rename = "fractional_allowed")]
    FractionalAllowed {
        #[serde(rename = "lot_size")]
        lot_size: Quantity,
        #[serde(rename = "minimum_fraction")]
        minimum_fraction: Option<Fraction>,
    },
    #[serde(rename = "board_lot")]
    BoardLot {
        #[serde(rename = "lot_size")]
        lot_size: Quantity,
        #[serde(rename = "odd_lot_allowed")]
        odd_lot_allowed: bool,
    },
}

impl LotPolicy {
    /// Validates the lot size and the minimum fraction.
    pub fn validate(&self) -> Result<(), ModelError> {
        match self {
            Self::WholeLots { lot_size } => require_positive_quantity(*lot_size, "lot_size"),
            Self::FractionalAllowed {
                lot_size,
                minimum_fraction,
            } => {
                require_positive_quantity(*lot_size, "lot_size")?;
                if minimum_fraction.is_some_and(Fraction::is_zero) {
                    return Err(ModelError::NonPositiveQuantity {
                        field: "minimum_fraction",
                    });
                }
                Ok(())
            }
            Self::BoardLot { lot_size, .. } => require_positive_quantity(*lot_size, "lot_size"),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "policy")]
enum LotPolicyWire {
    #[serde(rename = "whole_lots")]
    WholeLots {
        #[serde(rename = "lot_size")]
        lot_size: Quantity,
    },
    #[serde(rename = "fractional_allowed")]
    FractionalAllowed {
        #[serde(rename = "lot_size")]
        lot_size: Quantity,
        #[serde(rename = "minimum_fraction")]
        minimum_fraction: Option<Fraction>,
    },
    #[serde(rename = "board_lot")]
    BoardLot {
        #[serde(rename = "lot_size")]
        lot_size: Quantity,
        #[serde(rename = "odd_lot_allowed")]
        odd_lot_allowed: bool,
    },
}

impl From<LotPolicyWire> for LotPolicy {
    fn from(wire: LotPolicyWire) -> Self {
        match wire {
            LotPolicyWire::WholeLots { lot_size } => Self::WholeLots { lot_size },
            LotPolicyWire::FractionalAllowed {
                lot_size,
                minimum_fraction,
            } => Self::FractionalAllowed {
                lot_size,
                minimum_fraction,
            },
            LotPolicyWire::BoardLot {
                lot_size,
                odd_lot_allowed,
            } => Self::BoardLot {
                lot_size,
                odd_lot_allowed,
            },
        }
    }
}

impl<'de> Deserialize<'de> for LotPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let policy = Self::from(LotPolicyWire::deserialize(deserializer)?);
        policy.validate().map_err(de::Error::custom)?;
        Ok(policy)
    }
}

/// Whether a spot listing allows short sales.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ShortSalePolicy {
    #[serde(rename = "allowed")]
    Allowed,
    #[serde(rename = "not_applicable")]
    NotApplicable,
    #[serde(rename = "restricted")]
    Restricted,
}

impl ShortSalePolicy {
    /// Returns the fixed canonical label of this policy.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::NotApplicable => "not-applicable",
            Self::Restricted => "restricted",
        }
    }
}

/// The margin mode of a derivative listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MarginMode {
    #[serde(rename = "cross")]
    Cross,
    #[serde(rename = "isolated")]
    Isolated,
    #[serde(rename = "portfolio")]
    Portfolio,
}

impl MarginMode {
    /// Returns the fixed canonical label of this mode.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cross => "cross",
            Self::Isolated => "isolated",
            Self::Portfolio => "portfolio",
        }
    }
}

/// The margin requirements of a derivative listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MarginProfile {
    #[serde(rename = "mode")]
    pub mode: MarginMode,
    #[serde(rename = "initial_margin")]
    pub initial_margin: Fraction,
    #[serde(rename = "maintenance_margin")]
    pub maintenance_margin: Fraction,
    #[serde(rename = "asset")]
    pub asset: AssetId,
}

impl MarginProfile {
    /// Builds a margin profile that keeps maintenance below initial margin.
    pub fn new(
        mode: MarginMode,
        initial_margin: Fraction,
        maintenance_margin: Fraction,
        asset: AssetId,
    ) -> Result<Self, ModelError> {
        if initial_margin.is_zero() {
            return Err(ModelError::NonPositiveQuantity {
                field: "margin.initial_margin",
            });
        }
        if maintenance_margin >= initial_margin {
            return Err(ModelError::TradingRulesMismatch {
                field: "margin.maintenance_margin",
            });
        }
        Ok(Self {
            mode,
            initial_margin,
            maintenance_margin,
            asset,
        })
    }
}

#[derive(Deserialize)]
struct MarginProfileWire {
    #[serde(rename = "mode")]
    mode: MarginMode,
    #[serde(rename = "initial_margin")]
    initial_margin: Fraction,
    #[serde(rename = "maintenance_margin")]
    maintenance_margin: Fraction,
    #[serde(rename = "asset")]
    asset: AssetId,
}

impl<'de> Deserialize<'de> for MarginProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MarginProfileWire::deserialize(deserializer)?;
        Self::new(
            wire.mode,
            wire.initial_margin,
            wire.maintenance_margin,
            wire.asset,
        )
        .map_err(de::Error::custom)
    }
}

/// One leverage tier of a perpetual listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LeverageTier {
    #[serde(rename = "notional_cap")]
    pub notional_cap: Quantity,
    #[serde(rename = "max_leverage")]
    pub max_leverage: u32,
    #[serde(rename = "maintenance_margin")]
    pub maintenance_margin: Fraction,
}

impl LeverageTier {
    /// Builds a leverage tier with a positive cap and leverage.
    pub fn new(
        notional_cap: Quantity,
        max_leverage: u32,
        maintenance_margin: Fraction,
    ) -> Result<Self, ModelError> {
        require_positive_quantity(notional_cap, "leverage.notional_cap")?;
        if max_leverage == 0 {
            return Err(ModelError::ZeroValue {
                field: "leverage.max_leverage",
            });
        }
        Ok(Self {
            notional_cap,
            max_leverage,
            maintenance_margin,
        })
    }
}

#[derive(Deserialize)]
struct LeverageTierWire {
    #[serde(rename = "notional_cap")]
    notional_cap: Quantity,
    #[serde(rename = "max_leverage")]
    max_leverage: u32,
    #[serde(rename = "maintenance_margin")]
    maintenance_margin: Fraction,
}

impl<'de> Deserialize<'de> for LeverageTier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LeverageTierWire::deserialize(deserializer)?;
        Self::new(
            wire.notional_cap,
            wire.max_leverage,
            wire.maintenance_margin,
        )
        .map_err(de::Error::custom)
    }
}

/// How often a perpetual listing funds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "interval")]
pub enum FundingInterval {
    #[serde(rename = "fixed")]
    Fixed {
        #[serde(rename = "interval_millis")]
        interval_millis: UnixMillis,
    },
    #[serde(rename = "adaptive")]
    Adaptive,
}

impl FundingInterval {
    /// Validates the funding interval.
    pub fn validate(&self) -> Result<(), ModelError> {
        if let Self::Fixed { interval_millis } = self {
            require_non_negative_time(*interval_millis, "funding.interval_millis")?;
            if *interval_millis == 0 {
                return Err(ModelError::NonPositiveQuantity {
                    field: "funding.interval_millis",
                });
            }
        }
        Ok(())
    }

    /// Returns the fixed canonical label of this interval.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fixed { .. } => "fixed",
            Self::Adaptive => "adaptive",
        }
    }
}

/// The funding schedule of a perpetual listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FundingSchedule {
    #[serde(rename = "interval")]
    pub interval: FundingInterval,
    #[serde(rename = "funding_rate_cap")]
    pub funding_rate_cap: Option<Fraction>,
    #[serde(rename = "funding_rate_floor")]
    pub funding_rate_floor: Option<Fraction>,
    #[serde(rename = "asset")]
    pub asset: AssetId,
}

impl FundingSchedule {
    /// Builds a funding schedule with a valid interval and rate bounds.
    pub fn new(
        interval: FundingInterval,
        funding_rate_cap: Option<Fraction>,
        funding_rate_floor: Option<Fraction>,
        asset: AssetId,
    ) -> Result<Self, ModelError> {
        interval.validate()?;
        if let (Some(cap), Some(floor)) = (funding_rate_cap, funding_rate_floor)
            && floor > cap
        {
            return Err(ModelError::TradingRulesMismatch {
                field: "funding.rate_bounds",
            });
        }
        Ok(Self {
            interval,
            funding_rate_cap,
            funding_rate_floor,
            asset,
        })
    }
}

#[derive(Deserialize)]
struct FundingScheduleWire {
    #[serde(rename = "interval")]
    interval: FundingInterval,
    #[serde(rename = "funding_rate_cap")]
    funding_rate_cap: Option<Fraction>,
    #[serde(rename = "funding_rate_floor")]
    funding_rate_floor: Option<Fraction>,
    #[serde(rename = "asset")]
    asset: AssetId,
}

impl<'de> Deserialize<'de> for FundingSchedule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FundingScheduleWire::deserialize(deserializer)?;
        Self::new(
            wire.interval,
            wire.funding_rate_cap,
            wire.funding_rate_floor,
            wire.asset,
        )
        .map_err(de::Error::custom)
    }
}

/// The spot-specific rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpotTradingRules {
    #[serde(rename = "lot_policy")]
    pub lot_policy: LotPolicy,
    #[serde(rename = "short_sale")]
    pub short_sale: ShortSalePolicy,
}

impl SpotTradingRules {
    /// Builds a validated spot rule set.
    pub fn new(lot_policy: LotPolicy, short_sale: ShortSalePolicy) -> Result<Self, ModelError> {
        lot_policy.validate()?;
        Ok(Self {
            lot_policy,
            short_sale,
        })
    }
}

#[derive(Deserialize)]
struct SpotTradingRulesWire {
    #[serde(rename = "lot_policy")]
    lot_policy: LotPolicy,
    #[serde(rename = "short_sale")]
    short_sale: ShortSalePolicy,
}

impl<'de> Deserialize<'de> for SpotTradingRules {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SpotTradingRulesWire::deserialize(deserializer)?;
        Self::new(wire.lot_policy, wire.short_sale).map_err(de::Error::custom)
    }
}

/// The margin requirements and leverage ladder of one perpetual contract.
///
/// A venue may publish these outside its instrument record — as the contract's
/// own position tiers — in which case this build carries the listing with an
/// unloaded [`PerpetualRisk`] instead of reading them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PerpetualMarginTerms {
    #[serde(rename = "margin")]
    pub margin: MarginProfile,
    #[serde(rename = "leverage")]
    pub leverage: Vec<LeverageTier>,
}

impl PerpetualMarginTerms {
    /// Builds margin terms with a non-empty ladder of strictly rising caps.
    pub fn new(margin: MarginProfile, leverage: Vec<LeverageTier>) -> Result<Self, ModelError> {
        if leverage.is_empty() {
            return Err(ModelError::EmptyCollection {
                field: "perpetual.leverage",
            });
        }
        if leverage
            .windows(2)
            .any(|pair| pair[0].notional_cap >= pair[1].notional_cap)
        {
            return Err(ModelError::UnsortedCollection {
                field: "perpetual.leverage",
            });
        }
        Ok(Self { margin, leverage })
    }
}

#[derive(Deserialize)]
struct PerpetualMarginTermsWire {
    #[serde(rename = "margin")]
    margin: MarginProfile,
    #[serde(rename = "leverage")]
    leverage: Vec<LeverageTier>,
}

impl<'de> Deserialize<'de> for PerpetualMarginTerms {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PerpetualMarginTermsWire::deserialize(deserializer)?;
        Self::new(wire.margin, wire.leverage).map_err(de::Error::custom)
    }
}

/// What is known about the margin terms of one perpetual listing.
///
/// A venue that publishes them separately from the instrument record leaves a
/// listing that is otherwise complete carrying them not yet read; a venue that
/// publishes them inline carries them read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "terms")]
pub enum PerpetualRisk {
    /// The venue publishes them outside the instrument record, not yet read.
    #[serde(rename = "unloaded")]
    Unloaded,
    /// The terms the venue published.
    #[serde(rename = "published")]
    Published(PerpetualMarginTerms),
}

/// The perpetual-specific rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PerpetualTradingRules {
    #[serde(rename = "risk")]
    pub risk: PerpetualRisk,
    #[serde(rename = "funding")]
    pub funding: FundingSchedule,
    #[serde(rename = "open_interest_cap")]
    pub open_interest_cap: Option<Quantity>,
}

impl PerpetualTradingRules {
    /// Validates the funding schedule.
    ///
    /// The margin terms are validated where they are built: an unloaded ladder
    /// is a state of the venue's data, not a defect.
    pub fn validate(&self) -> Result<(), ModelError> {
        self.funding.interval.validate()
    }
}

#[derive(Deserialize)]
struct PerpetualTradingRulesWire {
    #[serde(rename = "risk")]
    risk: PerpetualRisk,
    #[serde(rename = "funding")]
    funding: FundingSchedule,
    #[serde(rename = "open_interest_cap")]
    open_interest_cap: Option<Quantity>,
}

impl<'de> Deserialize<'de> for PerpetualTradingRules {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PerpetualTradingRulesWire::deserialize(deserializer)?;
        let rules = Self {
            risk: wire.risk,
            funding: wire.funding,
            open_interest_cap: wire.open_interest_cap,
        };
        rules.validate().map_err(de::Error::custom)?;
        Ok(rules)
    }
}

/// The future-specific rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FutureTradingRules {
    #[serde(rename = "margin")]
    pub margin: MarginProfile,
    #[serde(rename = "delivery_window")]
    pub delivery_window: Option<TimeWindow>,
    #[serde(rename = "position_limit")]
    pub position_limit: Option<Quantity>,
}

impl FutureTradingRules {
    /// Builds a validated future rule set.
    pub fn new(
        margin: MarginProfile,
        delivery_window: Option<TimeWindow>,
        position_limit: Option<Quantity>,
    ) -> Result<Self, ModelError> {
        if let Some(limit) = position_limit {
            require_positive_quantity(limit, "future.position_limit")?;
        }
        Ok(Self {
            margin,
            delivery_window,
            position_limit,
        })
    }
}

#[derive(Deserialize)]
struct FutureTradingRulesWire {
    #[serde(rename = "margin")]
    margin: MarginProfile,
    #[serde(rename = "delivery_window")]
    delivery_window: Option<TimeWindow>,
    #[serde(rename = "position_limit")]
    position_limit: Option<Quantity>,
}

impl<'de> Deserialize<'de> for FutureTradingRules {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = FutureTradingRulesWire::deserialize(deserializer)?;
        Self::new(wire.margin, wire.delivery_window, wire.position_limit).map_err(de::Error::custom)
    }
}

/// The option-specific rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OptionTradingRules {
    #[serde(rename = "margin")]
    pub margin: MarginProfile,
    #[serde(rename = "exercise_cutoff")]
    pub exercise_cutoff: Option<UnixMillis>,
    #[serde(rename = "assignment_cutoff")]
    pub assignment_cutoff: Option<UnixMillis>,
}

impl OptionTradingRules {
    /// Builds a validated option rule set.
    pub fn new(
        margin: MarginProfile,
        exercise_cutoff: Option<UnixMillis>,
        assignment_cutoff: Option<UnixMillis>,
    ) -> Result<Self, ModelError> {
        if let Some(cutoff) = exercise_cutoff {
            require_non_negative_time(cutoff, "option.exercise_cutoff")?;
        }
        if let Some(cutoff) = assignment_cutoff {
            require_non_negative_time(cutoff, "option.assignment_cutoff")?;
        }
        Ok(Self {
            margin,
            exercise_cutoff,
            assignment_cutoff,
        })
    }
}

#[derive(Deserialize)]
struct OptionTradingRulesWire {
    #[serde(rename = "margin")]
    margin: MarginProfile,
    #[serde(rename = "exercise_cutoff")]
    exercise_cutoff: Option<UnixMillis>,
    #[serde(rename = "assignment_cutoff")]
    assignment_cutoff: Option<UnixMillis>,
}

impl<'de> Deserialize<'de> for OptionTradingRules {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OptionTradingRulesWire::deserialize(deserializer)?;
        Self::new(wire.margin, wire.exercise_cutoff, wire.assignment_cutoff)
            .map_err(de::Error::custom)
    }
}

/// The outcome-specific rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeTradingRules {
    #[serde(rename = "order_book")]
    pub order_book: bool,
    #[serde(rename = "negative_risk_conversion")]
    pub negative_risk_conversion: bool,
}

/// The product-specific rules of one listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "rules")]
pub enum ProductTradingRules {
    #[serde(rename = "spot")]
    Spot(SpotTradingRules),
    #[serde(rename = "perpetual")]
    Perpetual(PerpetualTradingRules),
    #[serde(rename = "future")]
    Future(FutureTradingRules),
    #[serde(rename = "option")]
    Option(OptionTradingRules),
    #[serde(rename = "outcome")]
    Outcome(OutcomeTradingRules),
}

impl ProductTradingRules {
    /// Validates the product-specific rules.
    pub fn validate(&self) -> Result<(), ModelError> {
        match self {
            Self::Spot(rules) => rules.lot_policy.validate(),
            Self::Perpetual(rules) => rules.validate(),
            Self::Future(_) | Self::Option(_) | Self::Outcome(_) => Ok(()),
        }
    }

    /// Returns the instrument kind these rules belong to.
    pub const fn instrument_kind(&self) -> InstrumentKind {
        match self {
            Self::Spot(_) => InstrumentKind::Spot,
            Self::Perpetual(_) => InstrumentKind::Perpetual,
            Self::Future(_) => InstrumentKind::Future,
            Self::Option(_) => InstrumentKind::Option,
            Self::Outcome(_) => InstrumentKind::Outcome,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "rules")]
enum ProductTradingRulesWire {
    #[serde(rename = "spot")]
    Spot(SpotTradingRules),
    #[serde(rename = "perpetual")]
    Perpetual(PerpetualTradingRules),
    #[serde(rename = "future")]
    Future(FutureTradingRules),
    #[serde(rename = "option")]
    Option(OptionTradingRules),
    #[serde(rename = "outcome")]
    Outcome(OutcomeTradingRules),
}

impl From<ProductTradingRulesWire> for ProductTradingRules {
    fn from(wire: ProductTradingRulesWire) -> Self {
        match wire {
            ProductTradingRulesWire::Spot(rules) => Self::Spot(rules),
            ProductTradingRulesWire::Perpetual(rules) => Self::Perpetual(rules),
            ProductTradingRulesWire::Future(rules) => Self::Future(rules),
            ProductTradingRulesWire::Option(rules) => Self::Option(rules),
            ProductTradingRulesWire::Outcome(rules) => Self::Outcome(rules),
        }
    }
}

impl<'de> Deserialize<'de> for ProductTradingRules {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let rules = Self::from(ProductTradingRulesWire::deserialize(deserializer)?);
        rules.validate().map_err(de::Error::custom)?;
        Ok(rules)
    }
}
