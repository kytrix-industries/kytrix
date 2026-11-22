//! The probability chart: one canvas, and the layers it is drawn in.
//!
//! A prediction event is a handful of lines over time, so the chart is a canvas
//! rather than a chart engine. Its parts change at different rates and live in
//! separate modules:
//!
//! - [`geometry`] is where everything lands: the plot inside the node's box, the two
//!   domains, the ticks, and the decimation that folds a series into at most one
//!   point per pixel column. Nothing there paints or reads a window.
//! - [`hover`] is the pointer's arithmetic: the sample a time snaps to, the step
//!   value at that sample, and where the label column and its boxes go.
//! - [`text`] turns values into shaped lines. The cache it holds is why a frame that
//!   moved the pointer by a pixel re-shapes nothing it shaped before; a handle whose
//!   atlas lease is gone is re-shaped instead of drawn from a stale texture.
//! - [`paint`] draws one prepared frame; [`canvas`] owns the canvas node, the
//!   prepared-frame cache and the steady-state short circuit.
//!
//! The pointer and the pulse phase are deliberately absent from the prepared cache
//! key: they change every frame, and everything they affect is drawn by [`paint`]
//! from the prepared frame.

pub(crate) mod canvas;
pub(crate) mod geometry;
pub(crate) mod hover;
pub(crate) mod paint;
pub(crate) mod text;

#[cfg(test)]
mod canvas_tests;

#[cfg(test)]
mod geometry_tests;

#[cfg(test)]
mod hover_tests;

#[cfg(test)]
mod paint_tests;

#[cfg(test)]
mod text_tests;
