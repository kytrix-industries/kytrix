//! The prediction order book: one leg's resting depth, grouped for the panel.
//!
//! A book is read the same way a chart is: a page that states it at an instant,
//! then the venue's channel that keeps it current. The three concerns change at
//! different rates, so they live in separate modules:
//!
//! - [`model`] is pure arithmetic and labels: buckets, cumulative notional,
//!   cumulative depth and how a number is read in a prediction book.
//! - [`service`] owns the protocol, the stream key and the task that joins the
//!   page and the channel, including the order in which they are applied.
//! - [`panel`] is the view's own state for drawing the ladder: the grouping, the
//!   depth easing, the scroll place a reader left it at, and the one list that
//!   virtualizes the rows.

pub(crate) mod model;
pub(crate) mod panel;
pub(crate) mod service;

pub(crate) use model::*;
pub(crate) use panel::*;
pub(crate) use service::*;

#[cfg(test)]
mod model_tests;

#[cfg(test)]
mod panel_tests;

#[cfg(test)]
mod service_tests;
