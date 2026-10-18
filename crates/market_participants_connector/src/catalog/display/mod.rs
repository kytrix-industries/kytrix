//! The connector side of the display contract: publication, catalog and resolver.
//!
//! [`catalog`] owns the immutable [`DisplayCatalog`](catalog::DisplayCatalog) and
//! the [`DisplayResolver`](catalog::DisplayResolver) that reads it,
//! [`publication`] owns the one pass that builds a catalog out of a publication,
//! and [`types`] owns the typed rejection [`DisplayError`](types::DisplayError).
//! Nothing here formats an instrument of its own: every name comes from
//! `market_participants_model::display`, and this module only decides the catalogue-global
//! questions that need the whole generation — the code of every asset, the name of
//! every instrument and the name of every listing.

pub mod catalog;
pub mod publication;
pub mod types;

#[cfg(test)]
mod display_catalog_tests;
