//! The prediction order book: one leg's resting depth, grouped for the panel.
//!
//! A book is read the same way a chart is: a page that states it at an instant,
//! then the venue's channel that keeps it current. The two halves change at
//! different rates, so they live in separate modules:
//!
//! - [`model`] is pure arithmetic and labels: buckets, cumulative notional,
//!   cumulative depth and how a number is read in a prediction book.
//! - [`service`] owns the protocol, the stream key and the task that joins the
//!   page and the channel, including the order in which they are applied.

pub(crate) mod model;
pub(crate) mod service;

pub(crate) use model::*;
pub(crate) use service::*;

#[cfg(test)]
mod model_tests;

#[cfg(test)]
mod service_tests;
