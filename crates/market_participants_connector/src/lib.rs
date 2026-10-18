//! Application-facing market-participants capability root, its network boundary and the
//! shared catalogue.
//!
//! [`connector::MarketParticipantsConnector`] owns one gateway, connection pool, registry,
//! catalogue and runtime projection for the application's market-participants
//! capabilities, and the `account`, `market`, and `trading` handles are
//! cheap-to-clone siblings that share that state and have no dependency on one
//! another.
//!
//! The venue dimension lives in [`venue`]: [`venue::definition`] is the static
//! table of the venues this build serves and the loaders that produce their
//! scopes, and [`venue::loaded`] is the one routing view a request is admitted
//! against. The directory dimension lives in [`catalog`]: one published
//! generation of every loaded venue's listings, which every read of an
//! instrument is answered from.

pub mod account;
pub(crate) mod adapter;
mod capability;
pub mod catalog;
pub(crate) mod client;
pub(crate) mod connection;
pub mod connector;
pub mod error;
pub mod market;
pub(crate) mod market_participants;
pub(crate) mod protocol;
pub mod routing;
pub mod runtime;
pub mod trading;
pub mod venue;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
