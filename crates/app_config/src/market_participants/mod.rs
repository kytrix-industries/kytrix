//! Persisted market-participant settings and their structural validation.
//!
//! These values are stored with the rest of the application settings. Resolving
//! credentials, selecting proxy routes, and managing adapters, connections, or
//! runtime generations stay outside this crate in `market_participants_connector`.

pub mod credentials;
pub mod error;
#[cfg(feature = "test-support")]
pub mod fixture;
pub mod origins;
pub mod settings;
