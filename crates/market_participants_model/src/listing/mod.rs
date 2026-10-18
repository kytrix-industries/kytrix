//! Venue routes, listings, native identifiers and venue trading rules.
//!
//! A [`listing_impl::VenueListing`] is one tradable implementation of an
//! economic [`crate::instrument::Instrument`] on one venue;
//! [`listing_impl::ListingSnapshot`] is its current rules and status. Neither
//! tickers nor native identifiers take part in the economic instrument identity,
//! and the listing identity is derived only from its validated continuity terms
//! and its route.

pub mod continuity;
pub mod continuity_series;
pub mod identifiers;
pub mod listing_impl;
pub mod native;
pub mod navigation;
pub mod route;
pub mod rules;
pub mod venue;

#[cfg(test)]
mod fixtures_tests;

#[cfg(test)]
mod identity_tests;

#[cfg(test)]
mod rules_tests;
