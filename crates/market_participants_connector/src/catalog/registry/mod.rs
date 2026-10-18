//! The canonical identity registries.
//!
//! The asset, reference and outcome registries are the only place where a
//! canonical identity is created for a catalog batch. An identity is derived from
//! validated canonical terms through the versioned deterministic encoding of
//! `market_participants_model`; nothing here allocates a process-local id, and two
//! registries handed the same terms always produce the same identity. A source
//! reaches them through the copy-on-write staging view of
//! [`staging::CatalogStagingHandle`], so an identity that a refused batch
//! produced never becomes visible to anybody.

pub mod asset;
pub mod outcome;
pub mod reference;
pub mod staging;
