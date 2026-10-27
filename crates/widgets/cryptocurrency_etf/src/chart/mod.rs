//! The chart side of the ETF pane: one canvas, and the four layers it is drawn in.
//!
//! A chart has the same three parts, separated here because they change at
//! different times:
//!
//! - **what it plots** is replaced when a snapshot arrives, which is at most twice
//!   a day;
//! - **what it says** — the axis labels, the dates, the tooltips — is derived from
//!   that snapshot and from the active locale;
//! - **what it looks like** is laid out from the box the pane gives it, and is
//!   re-derived whenever that box changes or the pointer moves.
//!
//! The canvas draws its axes, dates, crosshair and tooltip itself rather than
//! assembling elements: a chart's labels move with its data, and a tooltip that
//! followed the pointer through a popover layer would escape the clipping that keeps
//! it inside the plot.
//!
//! `axes` is where everything lands, `text` is what turns values into shaped lines,
//! `palette` is the chart's fixed geometry, `data` is the resolved view, `paint` is
//! the two passes one frame runs, and `canvas` is the element and the state it keeps
//! between frames.

mod axes;
mod canvas;
mod data;
mod paint;
mod palette;
mod text;

pub(crate) use canvas::CryptocurrencyEtfChart;
pub(crate) use data::Metrics;
pub(crate) use palette::flow;

#[cfg(test)]
mod axes_tests;

#[cfg(test)]
mod canvas_tests;

#[cfg(test)]
mod data_tests;

#[cfg(test)]
mod paint_tests;

#[cfg(test)]
mod palette_tests;

#[cfg(test)]
mod text_tests;

#[cfg(test)]
pub(crate) mod test_support;
