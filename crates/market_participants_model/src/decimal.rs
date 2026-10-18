//! Fixed-point decimal values shared by every market-participants model number.
//!
//! All decimals use 18 fractional digits and never use floating point. The
//! canonical decimal representation has no trailing fractional zeroes, which is
//! also the representation used by canonical identity encoding.

use std::{fmt, num::NonZeroU128, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use smol_str::{SmolStr, format_smolstr};

use crate::{
    error::ModelError,
    identifier::{CanonicalEncoder, CanonicalTerms},
};

const DECIMAL_SCALE: u128 = 1_000_000_000_000_000_000;
const DECIMAL_PLACES: usize = 18;

fn parse_unsigned_decimal(value: &str) -> Result<u128, ModelError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ModelError::EmptyDecimal);
    }
    if value.starts_with('-') {
        return Err(ModelError::NegativeDecimal);
    }
    if value.starts_with('+') || value.matches('.').count() > 1 {
        return Err(ModelError::InvalidDecimal);
    }

    let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
    if integer.is_empty() && fraction.is_empty() {
        return Err(ModelError::EmptyDecimal);
    }
    if !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ModelError::InvalidDecimal);
    }
    if fraction.len() > DECIMAL_PLACES {
        return Err(ModelError::DecimalPrecisionExceeded {
            maximum: DECIMAL_PLACES,
        });
    }

    let integer = if integer.is_empty() {
        0
    } else {
        integer
            .parse::<u128>()
            .map_err(|_| ModelError::DecimalOverflow)?
    };
    let fraction = if fraction.is_empty() {
        0
    } else {
        let parsed = fraction
            .parse::<u128>()
            .map_err(|_| ModelError::InvalidDecimal)?;
        parsed
            .checked_mul(10_u128.pow((DECIMAL_PLACES - fraction.len()) as u32))
            .ok_or(ModelError::DecimalOverflow)?
    };

    integer
        .checked_mul(DECIMAL_SCALE)
        .and_then(|scaled| scaled.checked_add(fraction))
        .ok_or(ModelError::DecimalOverflow)
}

fn parse_signed_decimal(value: &str) -> Result<i128, ModelError> {
    let value = value.trim();
    let (negative, magnitude) = match value.strip_prefix('-') {
        Some(magnitude) => (true, magnitude),
        None => (false, value),
    };
    let magnitude = parse_unsigned_decimal(magnitude)?;
    if negative {
        if magnitude == i128::MAX as u128 + 1 {
            return Ok(i128::MIN);
        }
        let magnitude = i128::try_from(magnitude).map_err(|_| ModelError::DecimalOverflow)?;
        magnitude.checked_neg().ok_or(ModelError::DecimalOverflow)
    } else {
        i128::try_from(magnitude).map_err(|_| ModelError::DecimalOverflow)
    }
}

fn format_unsigned_decimal(value: u128, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let integer = value / DECIMAL_SCALE;
    let fraction = value % DECIMAL_SCALE;
    if fraction == 0 {
        return write!(formatter, "{integer}");
    }
    let fraction = format!("{fraction:018}");
    write!(formatter, "{integer}.{}", fraction.trim_end_matches('0'))
}

fn format_signed_decimal(value: i128, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    if value.is_negative() {
        formatter.write_str("-")?;
    }
    format_unsigned_decimal(value.unsigned_abs(), formatter)
}

fn checked_multiply_scaled(left: u128, right: u128) -> Option<u128> {
    let left_integer = left / DECIMAL_SCALE;
    let left_fraction = left % DECIMAL_SCALE;
    let right_integer = right / DECIMAL_SCALE;
    let right_fraction = right % DECIMAL_SCALE;
    left_integer
        .checked_mul(right)?
        .checked_add(left_fraction.checked_mul(right_integer)?)?
        .checked_add(left_fraction.checked_mul(right_fraction)? / DECIMAL_SCALE)
}

/// Divides two 18-digit fixed-point values and keeps the scale of the quotient.
///
/// Scaling the numerator up front — the obvious `numerator * DECIMAL_SCALE /
/// denominator` — overflows for any numerator past a few hundred units, which is
/// every traded notional. The fraction is therefore produced one decimal digit at
/// a time out of the running remainder, so only the result itself has to fit.
fn checked_divide_scaled(numerator: u128, denominator: u128) -> Option<u128> {
    if denominator == 0 {
        return None;
    }

    let integer = numerator.checked_div(denominator)?;
    let result = integer.checked_mul(DECIMAL_SCALE)?;
    let mut remainder = numerator % denominator;
    let mut fraction = 0_u128;
    for _ in 0..DECIMAL_PLACES {
        remainder = remainder.checked_mul(10)?;
        fraction = fraction
            .checked_mul(10)?
            .checked_add(remainder.checked_div(denominator)?)?;
        remainder %= denominator;
    }

    result.checked_add(fraction)
}

fn signed_from_magnitude(magnitude: u128, negative: bool) -> Option<i128> {
    if negative {
        if magnitude == i128::MAX as u128 + 1 {
            Some(i128::MIN)
        } else {
            i128::try_from(magnitude).ok()?.checked_neg()
        }
    } else {
        i128::try_from(magnitude).ok()
    }
}

fn decimal_places(value: u128) -> usize {
    let mut fraction = value % DECIMAL_SCALE;
    if fraction == 0 {
        0
    } else {
        let mut trailing_zeroes = 0;
        while trailing_zeroes < DECIMAL_PLACES && fraction.is_multiple_of(10) {
            fraction /= 10;
            trailing_zeroes += 1;
        }
        DECIMAL_PLACES - trailing_zeroes
    }
}

fn significant_digits(value: u128) -> usize {
    if value == 0 {
        return 0;
    }
    let integer = value / DECIMAL_SCALE;
    let fraction = value % DECIMAL_SCALE;
    if integer > 0 {
        integer.ilog10() as usize + 1 + decimal_places(value)
    } else {
        let places = decimal_places(value);
        let mut divisor = 10_u128.pow((DECIMAL_PLACES - 1) as u32);
        let mut leading_zeroes = 0;
        while leading_zeroes < DECIMAL_PLACES && fraction < divisor {
            divisor /= 10;
            leading_zeroes += 1;
        }
        places.saturating_sub(leading_zeroes)
    }
}

macro_rules! unsigned_decimal {
    ($name:ident) => {
        #[doc = concat!("An unsigned decimal `", stringify!($name), "` with 18 fractional digits.")]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u128);

        impl $name {
            pub const ZERO: Self = Self(0);

            /// Creates a value from its 18-digit fixed-point representation.
            pub const fn from_inner(inner: u128) -> Self {
                Self(inner)
            }

            /// Returns the 18-digit fixed-point representation.
            pub const fn inner(self) -> u128 {
                self.0
            }

            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            pub const fn checked_add(self, other: Self) -> Option<Self> {
                match self.0.checked_add(other.0) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            pub const fn checked_sub(self, other: Self) -> Option<Self> {
                match self.0.checked_sub(other.0) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            /// Rounds downward to an exact multiple of `tick`.
            pub const fn checked_floor_to_tick(self, tick: Tick) -> Self {
                Self(self.0 - self.0 % tick.inner())
            }

            /// Rounds upward to an exact multiple of `tick`.
            pub const fn checked_ceil_to_tick(self, tick: Tick) -> Option<Self> {
                let remainder = self.0 % tick.inner();
                if remainder == 0 {
                    Some(self)
                } else {
                    match self.0.checked_add(tick.inner() - remainder) {
                        Some(value) => Some(Self(value)),
                        None => None,
                    }
                }
            }

            /// Returns the canonical number of fractional decimal places.
            pub fn decimal_places(self) -> usize {
                decimal_places(self.0)
            }

            /// Returns the number of significant digits in the canonical decimal value.
            pub fn significant_digits(self) -> usize {
                significant_digits(self.0)
            }

            /// Rounds to the nearest tick. Half-way values round upward.
            pub const fn checked_round_to_tick(self, tick: Tick) -> Option<Self> {
                let tick = tick.inner();
                let remainder = self.0 % tick;
                let base = self.0 - remainder;
                if remainder >= tick - remainder {
                    match base.checked_add(tick) {
                        Some(value) => Some(Self(value)),
                        None => None,
                    }
                } else {
                    Some(Self(base))
                }
            }

            /// Rounds to the nearest tick and preserves the tick's decimal precision.
            pub fn format_and_round_tick(self, tick: Tick) -> SmolStr {
                tick.format_unsigned(self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_unsigned_decimal(self.0, formatter)
            }
        }

        impl FromStr for $name {
            type Err = ModelError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_unsigned_decimal(value).map(Self)
            }
        }

        impl From<u8> for $name {
            fn from(value: u8) -> Self {
                Self(u128::from(value) * DECIMAL_SCALE)
            }
        }

        impl From<u16> for $name {
            fn from(value: u16) -> Self {
                Self(u128::from(value) * DECIMAL_SCALE)
            }
        }

        impl From<u32> for $name {
            fn from(value: u32) -> Self {
                Self(u128::from(value) * DECIMAL_SCALE)
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(u128::from(value) * DECIMAL_SCALE)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct DecimalVisitor;

                impl<'de> de::Visitor<'de> for DecimalVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a non-negative decimal string or integer")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        value.parse().map_err(de::Error::custom)
                    }

                    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        Ok(value.into())
                    }

                    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        let value = u64::try_from(value)
                            .map_err(|_| de::Error::custom(ModelError::NegativeDecimal))?;
                        Ok(value.into())
                    }
                }

                deserializer.deserialize_any(DecimalVisitor)
            }
        }
    };
}

macro_rules! signed_decimal {
    ($name:ident) => {
        #[doc = concat!("A signed decimal `", stringify!($name), "` with 18 fractional digits.")]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(i128);

        impl $name {
            pub const ZERO: Self = Self(0);

            /// Creates a value from its 18-digit fixed-point representation.
            pub const fn from_inner(inner: i128) -> Self {
                Self(inner)
            }

            /// Returns the 18-digit fixed-point representation.
            pub const fn inner(self) -> i128 {
                self.0
            }

            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            pub const fn is_negative(self) -> bool {
                self.0 < 0
            }

            pub const fn checked_add(self, other: Self) -> Option<Self> {
                match self.0.checked_add(other.0) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            pub const fn checked_sub(self, other: Self) -> Option<Self> {
                match self.0.checked_sub(other.0) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            /// Rounds to the nearest tick and preserves the tick's decimal precision.
            pub fn format_and_round_tick(self, tick: Tick) -> SmolStr {
                tick.format_signed(self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_signed_decimal(self.0, formatter)
            }
        }

        impl FromStr for $name {
            type Err = ModelError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_signed_decimal(value).map(Self)
            }
        }

        impl From<i64> for $name {
            fn from(value: i64) -> Self {
                Self(i128::from(value) * DECIMAL_SCALE as i128)
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(i128::from(value) * DECIMAL_SCALE as i128)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct DecimalVisitor;

                impl<'de> de::Visitor<'de> for DecimalVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a signed decimal string or integer")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        value.parse().map_err(de::Error::custom)
                    }

                    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        Ok(value.into())
                    }

                    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
                    where
                        E: de::Error,
                    {
                        Ok(value.into())
                    }
                }

                deserializer.deserialize_any(DecimalVisitor)
            }
        }
    };
}

unsigned_decimal!(Price);
unsigned_decimal!(Quantity);
unsigned_decimal!(Ratio);
signed_decimal!(SignedQuantity);
signed_decimal!(ProfitOrLoss);

impl Price {
    /// Calculates a quote-asset notional without losing fixed-point scale.
    pub fn checked_mul_quantity(self, quantity: Quantity) -> Option<Quantity> {
        checked_multiply_scaled(self.inner(), quantity.inner()).map(Quantity::from_inner)
    }

    /// Calculates a ratio of this price without losing fixed-point scale.
    pub fn checked_mul_ratio(self, ratio: Ratio) -> Option<Self> {
        checked_multiply_scaled(self.inner(), ratio.inner()).map(Self::from_inner)
    }
}

impl Quantity {
    /// Calculates a quote-asset notional without losing fixed-point scale.
    pub fn checked_mul_price(self, price: Price) -> Option<Quantity> {
        price.checked_mul_quantity(self)
    }

    /// Divides a quote-asset notional by a price and returns the base quantity it
    /// traded for.
    pub fn checked_div_price(self, price: Price) -> Option<Self> {
        checked_divide_scaled(self.inner(), price.inner()).map(Self::from_inner)
    }

    /// Divides a quote amount by a base quantity and returns a fixed-point price.
    pub fn checked_div_quantity(self, quantity: Quantity) -> Option<Price> {
        checked_divide_scaled(self.inner(), quantity.inner()).map(Price::from_inner)
    }
}

impl Ratio {
    /// Scales a quantity by this ratio without losing fixed-point scale.
    pub fn checked_mul_quantity(self, quantity: Quantity) -> Option<Quantity> {
        checked_multiply_scaled(self.inner(), quantity.inner()).map(Quantity::from_inner)
    }

    /// Scales a price by this ratio without losing fixed-point scale.
    pub fn checked_mul_price(self, price: Price) -> Option<Price> {
        checked_multiply_scaled(self.inner(), price.inner()).map(Price::from_inner)
    }
}

impl SignedQuantity {
    pub fn checked_neg(self) -> Option<Self> {
        self.inner().checked_neg().map(Self::from_inner)
    }

    /// Calculates a signed quote-asset notional without losing fixed-point scale.
    pub fn checked_mul_price(self, price: Price) -> Option<Self> {
        let magnitude = checked_multiply_scaled(self.inner().unsigned_abs(), price.inner())?;
        signed_from_magnitude(magnitude, self.inner().is_negative()).map(Self::from_inner)
    }

    /// Returns the magnitude of this signed quantity.
    pub const fn magnitude(self) -> Quantity {
        Quantity::from_inner(self.inner().unsigned_abs())
    }
}

impl ProfitOrLoss {
    /// Returns the magnitude of this profit or loss.
    pub const fn magnitude(self) -> Quantity {
        Quantity::from_inner(self.inner().unsigned_abs())
    }
}

impl From<SignedQuantity> for ProfitOrLoss {
    fn from(value: SignedQuantity) -> Self {
        Self::from_inner(value.inner())
    }
}

impl From<ProfitOrLoss> for SignedQuantity {
    fn from(value: ProfitOrLoss) -> Self {
        Self::from_inner(value.inner())
    }
}

/// A ratio constrained to `[0, 1]`, used for probabilities, settlement
/// fractions and relative price limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fraction(Ratio);

impl Fraction {
    pub const ZERO: Self = Self(Ratio::ZERO);
    pub const ONE: Self = Self(Ratio::from_inner(DECIMAL_SCALE));

    /// Validates a ratio as a fraction within `[0, 1]`.
    pub const fn new(ratio: Ratio) -> Result<Self, ModelError> {
        if ratio.inner() > DECIMAL_SCALE {
            Err(ModelError::FractionOutOfRange)
        } else {
            Ok(Self(ratio))
        }
    }

    /// Returns the underlying ratio.
    pub const fn ratio(self) -> Ratio {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    pub const fn is_one(self) -> bool {
        self.0.inner() == DECIMAL_SCALE
    }

    /// Returns `true` when the fraction is exactly `0` or `1`.
    pub const fn is_binary(self) -> bool {
        self.is_zero() || self.is_one()
    }

    /// Returns the complementary fraction `1 - self`.
    pub const fn complement(self) -> Self {
        Self(Ratio::from_inner(DECIMAL_SCALE - self.0.inner()))
    }
}

impl fmt::Display for Fraction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Fraction {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value.parse()?)
    }
}

impl TryFrom<Ratio> for Fraction {
    type Error = ModelError;

    fn try_from(value: Ratio) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Fraction> for Ratio {
    fn from(value: Fraction) -> Self {
        value.0
    }
}

impl Serialize for Fraction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fraction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Ratio::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// A ratio strictly greater than zero, used for conversion factors and share
/// ratios that are not bounded by one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositiveRatio(Ratio);

impl PositiveRatio {
    /// Validates a ratio as strictly positive.
    pub const fn new(ratio: Ratio) -> Result<Self, ModelError> {
        if ratio.is_zero() {
            Err(ModelError::NonPositiveRatio)
        } else {
            Ok(Self(ratio))
        }
    }

    /// Returns the underlying ratio.
    pub const fn ratio(self) -> Ratio {
        self.0
    }
}

impl fmt::Display for PositiveRatio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for PositiveRatio {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value.parse()?)
    }
}

impl TryFrom<Ratio> for PositiveRatio {
    type Error = ModelError;

    fn try_from(value: Ratio) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PositiveRatio> for Ratio {
    fn from(value: PositiveRatio) -> Self {
        value.0
    }
}

impl Serialize for PositiveRatio {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PositiveRatio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Ratio::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

impl CanonicalTerms for Ratio {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_u128(self.inner());
    }
}

impl CanonicalTerms for Fraction {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_u128(self.0.inner());
    }
}

impl CanonicalTerms for PositiveRatio {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_u128(self.0.inner());
    }
}

impl CanonicalTerms for Quantity {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_u128(self.inner());
    }
}

impl CanonicalTerms for Price {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_u128(self.inner());
    }
}

impl CanonicalTerms for SignedQuantity {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_i128(self.inner());
    }
}

/// A positive price or quantity increment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tick(NonZeroU128);

impl Tick {
    /// Creates a tick from its 18-digit fixed-point representation.
    pub const fn from_inner(inner: u128) -> Result<Self, ModelError> {
        match NonZeroU128::new(inner) {
            Some(inner) => Ok(Self(inner)),
            None => Err(ModelError::ZeroTick),
        }
    }

    pub const fn inner(self) -> u128 {
        self.0.get()
    }

    pub const fn decimal_places(self) -> usize {
        let mut trailing_zeroes = 0;
        let mut value = self.inner();
        while trailing_zeroes < DECIMAL_PLACES && value.is_multiple_of(10) {
            value /= 10;
            trailing_zeroes += 1;
        }
        DECIMAL_PLACES - trailing_zeroes
    }

    fn format_unsigned(self, value: u128) -> SmolStr {
        format_rounded_unsigned(value, self.inner(), self.decimal_places())
    }

    fn format_signed(self, value: i128) -> SmolStr {
        let magnitude = value.unsigned_abs();
        let formatted = self.format_unsigned(magnitude);
        let remainder = magnitude % self.inner();
        let rounds_to_zero = magnitude == remainder && remainder < self.inner() - remainder;
        if value.is_negative() && !rounds_to_zero {
            format_smolstr!("-{formatted}")
        } else {
            formatted
        }
    }
}

impl TryFrom<Quantity> for Tick {
    type Error = ModelError;

    fn try_from(value: Quantity) -> Result<Self, Self::Error> {
        Self::from_inner(value.inner())
    }
}

fn format_rounded_unsigned(value: u128, tick: u128, decimal_places: usize) -> SmolStr {
    let remainder = value % tick;
    let round_up = remainder >= tick - remainder;
    let base = value - remainder;
    let base_integer = base / DECIMAL_SCALE;
    let base_fractional = base % DECIMAL_SCALE;
    let added_integer = if round_up { tick / DECIMAL_SCALE } else { 0 };
    let added_fractional = if round_up { tick % DECIMAL_SCALE } else { 0 };
    let fractional_sum = base_fractional + added_fractional;
    let integer = base_integer + added_integer + fractional_sum / DECIMAL_SCALE;
    if decimal_places == 0 {
        return format_smolstr!("{integer}");
    }
    let fractional =
        fractional_sum % DECIMAL_SCALE / 10_u128.pow((DECIMAL_PLACES - decimal_places) as u32);
    format_smolstr!("{integer}.{fractional:0>decimal_places$}")
}

impl fmt::Display for Tick {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_unsigned_decimal(self.inner(), formatter)
    }
}

impl FromStr for Tick {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_inner(parse_unsigned_decimal(value)?)
    }
}

impl Serialize for Tick {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Tick {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

/// Unix timestamp in milliseconds.
pub type UnixMillis = i64;

/// Validates that a Unix millisecond timestamp is non-negative.
pub fn validate_timestamp(timestamp: UnixMillis, field: &'static str) -> Result<(), ModelError> {
    if timestamp < 0 {
        Err(ModelError::InvalidTimestamp { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        decimal::{
            DECIMAL_PLACES, Fraction, ModelError, PositiveRatio, Price, Quantity, Ratio,
            SignedQuantity, Tick,
        },
        identifier::CanonicalTerms,
    };

    #[test]
    fn decimal_should_round_trip_without_precision_loss() {
        let price = "340282366920938463463.374607431768211455"
            .parse::<Price>()
            .expect("maximum fixed-point value should parse");
        let json = serde_json::to_string(&price).expect("price should serialize");
        let decoded = serde_json::from_str::<Price>(&json).expect("price should deserialize");

        assert_eq!(decoded, price);
    }

    #[test]
    fn decimal_should_reject_excess_fractional_precision() {
        let error = "0.0000000000000000001"
            .parse::<Quantity>()
            .expect_err("nineteen fractional digits must fail");

        assert_eq!(
            error,
            ModelError::DecimalPrecisionExceeded {
                maximum: DECIMAL_PLACES
            }
        );
    }

    #[test]
    fn notional_should_not_overflow_only_the_intermediate_product() {
        let half = "0.5".parse::<Price>().expect("half should parse");
        let maximum = Quantity::from_inner(u128::MAX);

        assert_eq!(
            half.checked_mul_quantity(maximum),
            Some(Quantity::from_inner(u128::MAX / 2))
        );
    }

    #[test]
    fn notional_should_divide_back_into_the_base_quantity_it_traded() {
        let price = "2.5".parse::<Price>().expect("price should parse");
        let notional = "10".parse::<Quantity>().expect("notional should parse");

        assert_eq!(
            notional.checked_div_price(price),
            Some("4".parse().expect("base quantity should parse"))
        );
    }

    #[test]
    fn base_quantity_should_be_unknown_without_a_price() {
        let notional = "10".parse::<Quantity>().expect("notional should parse");

        assert_eq!(notional.checked_div_price(Price::ZERO), None);
    }

    /// A day of a liquid market trades a notional that scaling up front would
    /// overflow long before the quotient does.
    #[test]
    fn a_traded_notional_should_divide_by_a_price_without_overflowing() {
        let notional = "1000000000"
            .parse::<Quantity>()
            .expect("notional should parse");
        let price = "100000".parse::<Price>().expect("price should parse");

        assert_eq!(
            notional.checked_div_price(price),
            Some("10000".parse().expect("base quantity should parse"))
        );
        assert_eq!(
            notional
                .checked_div_quantity("10000".parse::<Quantity>().expect("quantity should parse")),
            Some(price)
        );
    }

    #[test]
    fn signed_notional_should_preserve_positive_and_negative_balances() {
        let price = "2.5".parse::<Price>().expect("price should parse");

        assert_eq!(
            (
                "4".parse::<SignedQuantity>()
                    .expect("positive balance should parse")
                    .checked_mul_price(price),
                "-4".parse::<SignedQuantity>()
                    .expect("negative balance should parse")
                    .checked_mul_price(price),
            ),
            (
                Some("10".parse().expect("positive value should parse")),
                Some("-10".parse().expect("negative value should parse")),
            )
        );
    }

    #[test]
    fn signed_notional_should_reject_overflow() {
        let balance = SignedQuantity::from_inner(i128::MAX);

        assert_eq!(balance.checked_mul_price(Price::from(2_u8)), None);
    }

    #[test]
    fn tick_should_reject_zero() {
        let error = "0".parse::<Tick>().expect_err("a zero tick must fail");

        assert_eq!(error, ModelError::ZeroTick);
    }

    #[test]
    fn fraction_should_stay_within_zero_and_one() {
        assert!(Fraction::new(Ratio::ZERO).is_ok());
        assert!(Fraction::new(Ratio::from(1_u8)).is_ok());
        assert_eq!(
            Fraction::new("1.000000000000000001".parse().expect("ratio")),
            Err(ModelError::FractionOutOfRange)
        );
        assert!(Fraction::new("0.5".parse().expect("ratio")).is_ok());
    }

    #[test]
    fn fraction_should_expose_binary_and_complement_semantics() {
        let half = Fraction::new("0.5".parse().expect("ratio")).expect("half is a fraction");

        assert!(half.complement() == half);
        assert!(
            Fraction::new(Ratio::from(1_u8))
                .expect("one is a fraction")
                .is_binary()
        );
        assert!(!half.is_binary());
        assert_eq!(Fraction::ZERO.complement(), Fraction::ONE);
    }

    #[test]
    fn positive_ratio_should_reject_zero_but_allow_values_above_one() {
        assert_eq!(
            PositiveRatio::new(Ratio::ZERO),
            Err(ModelError::NonPositiveRatio)
        );
        assert!(PositiveRatio::new("2.5".parse().expect("ratio")).is_ok());
    }

    #[test]
    fn ratio_should_allow_statistics_values_above_one() {
        let ratio = "2.5".parse::<Ratio>().expect("ratio should parse");

        assert!(ratio > Ratio::from(1_u8));
    }

    #[test]
    fn canonical_encoding_should_use_the_fixed_point_representation() {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        "1.500"
            .parse::<Ratio>()
            .expect("ratio")
            .encode(&mut encoder);

        assert_eq!(encoder.as_bytes().len(), 16);
        assert_eq!(
            encoder.as_bytes(),
            &1_500_000_000_000_000_000_u128.to_be_bytes()
        );
        assert_eq!(
            encoder.as_bytes(),
            &"1.5".parse::<Ratio>().expect("ratio").inner().to_be_bytes(),
            "the canonical form carries no trailing fractional zeroes"
        );
    }

    #[test]
    fn tick_should_be_derivable_from_a_quantity() {
        let quantity = "0.01".parse::<Quantity>().expect("quantity should parse");

        assert_eq!(
            Tick::try_from(quantity)
                .expect("tick should be valid")
                .to_string(),
            "0.01"
        );
    }
}
