//! The venue dimension: what this build speaks, and what a load has committed.
//!
//! [`definition`] is the static table — a venue identity, the shape of its
//! connection configuration, the capability names its loader can produce and the
//! loader itself. `specs` is the crate-private scope table that table's loaders
//! answer from. [`config`] is what one load runs on and how it is cancelled,
//! [`metadata`] what a commit publishes beside its scope set, and [`loaded`] the
//! runtime routing view every request is admitted against.

pub mod config;
pub mod definition;
pub mod loaded;
pub mod metadata;
pub(crate) mod specs;

#[cfg(test)]
mod definition_tests;
#[cfg(test)]
mod loaded_tests;
#[cfg(test)]
pub(crate) mod test_support;
