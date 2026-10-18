//! Market-participant-neutral canonical instrument, listing and market model.
//!
//! This crate is the leaf of the market-participants dependency graph: it has no
//! networking, adapter, capability, UI or runtime dependencies. It owns the
//! canonical identities, the five product families, outcome relations, venue
//! routes and listings, venue trading rules and the unit-carrying public market
//! values. Adapters convert venue-specific wire data into these types.

#[macro_use]
mod macros;

pub mod asset;
pub mod decimal;
pub mod display;
pub mod error;
pub mod identifier;
pub mod instrument;
pub mod listing;
pub mod market;
pub mod outcome;
pub mod reference;
pub mod routing;
pub mod settlement;
