//! The chart side of the statistics widgets: the three chart types, and the
//! caches one frame of any of them reuses.
//!
//! A chart of any of the five series has the same three parts, separated here
//! because they change at different times:
//!
//! - **what it plots** is replaced when a snapshot arrives, which is at most once
//!   a minute;
//! - **what it says** — the axis labels, the legend, the tooltips — is derived
//!   from that snapshot and from the active locale;
//! - **what it looks like** is laid out from the box the pane gives it, and is
//!   re-derived whenever that box changes or the pointer moves.
//!
//! The canvases draw their axes, legends, crosshair and tooltips themselves
//! rather than assembling elements: a chart's labels move with its data, and a
//! tooltip that followed the pointer through a popover layer would escape the
//! clipping that keeps it inside the plot.
//!
//! The three types differ only in their data, so everything that is the same for
//! all of them lives in `canvas`, and each type answers only the questions that
//! are its own.

mod axes;
pub(crate) mod canvas;
pub(crate) mod open_interest;
mod paint;
mod palette;
pub(crate) mod ratio;
pub(crate) mod taker_volume;
mod text;

pub(crate) use canvas::{ChartData, ChartDispatch};

#[cfg(test)]
mod axes_tests;

#[cfg(test)]
mod canvas_tests;

#[cfg(test)]
mod open_interest_tests;

#[cfg(test)]
mod paint_tests;

#[cfg(test)]
mod palette_tests;

#[cfg(test)]
mod text_tests;
