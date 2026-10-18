//! The human-readable instrument display strings of the display contract.
//!
//! The display layer is a pure formatter over already validated canonical terms.
//! It has no connector, route snapshot, UI or runtime dependency, it never reads
//! a native symbol to guess an economic clause, and it owns exactly one
//! implementation of the field order, the numeric format and the omission rules.
//!
//! [`lexical`] owns the display pipeline: the normalization of a dynamic component
//! and the user-visible route formatter. [`asset`] turns canonical asset terms
//! into the one display code an asset has, [`values`] holds the typed display
//! values and the typed formatter error, [`details`] holds the typed details of
//! the five product families, and [`decimal`] and [`timestamp`] render the
//! canonical decimals and UTC times. [`grammar`] assembles the five product
//! grammars on top of these primitives.

pub mod asset;
pub mod decimal;
pub mod details;
pub mod grammar;
pub mod lexical;
pub mod timestamp;
pub mod values;

#[cfg(test)]
mod asset_tests;

#[cfg(test)]
mod decimal_tests;

#[cfg(test)]
mod grammar_tests;

#[cfg(test)]
mod lexical_tests;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod timestamp_tests;
