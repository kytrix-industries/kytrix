//! Unit-carrying public market and selection DTOs.
//!
//! Every normalized market value declares what it measures: a price carries its
//! quote asset and basis, and a quantity carries its `QuantityUnit`. Monetary
//! amounts must be expressed as `QuantityUnit::Notional { asset }`; asset
//! quantities must never be expressed as a notional.

pub mod account;
pub mod book;
pub mod price;
pub mod selection;
pub mod units;

#[cfg(test)]
mod market_tests;
