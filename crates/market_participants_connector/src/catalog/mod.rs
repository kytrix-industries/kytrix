//! The shared catalogue.
//!
//! One catalogue per market participants root: the [`store::Catalog`] holds the registered
//! catalog sources and routes and the one published [`generation::CatalogGeneration`],
//! which carries every loaded route's listings, the canonical identity
//! registries those listings name and the display catalogue derived from them.
//! A read is answered from the published generation alone — [`resolved`] holds
//! the values a caller gets back — and the only writer is the venue load
//! transaction of [`crate::connector`], which publishes one venue's whole
//! commit through the crate-private verbs of the store.
//!
//! The wire side lives in the crate-private `market_participants` module: the adapter-backed
//! [`source::CatalogSource`] a load fetches a route's directory through.

pub mod batch;
pub mod display;
pub mod error;
pub mod generation;
pub mod registry;
pub mod resolved;
pub mod source;
pub mod store;

pub(crate) mod market_participants;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod display_read_tests;
#[cfg(test)]
mod source_tests;
#[cfg(test)]
mod store_tests;
