//! The probability chart: one canvas, and the layers it is drawn in.
//!
//! A prediction event is a handful of lines over time, so the chart is a canvas
//! rather than a chart engine. Its four parts change at different rates and live in
//! separate modules:
//!
//! - [`geometry`] is where everything lands: the plot inside the element's box, the
//!   two domains, the ticks, and the decimation that folds a series into at most one
//!   point per pixel column. Nothing there paints or reads a window.
//! - [`hover`] is the pointer's arithmetic: the sample a time snaps to, the step
//!   value at that sample, and where the label column and its boxes go.
//! - [`text`] turns values into shaped lines. The cache it holds is why a frame that
//!   moved the pointer by a pixel re-shapes nothing it shaped before.
//! - [`paint`] draws one prepared frame; [`canvas`] owns the element, the options it
//!   is handed, and the prepaint cache that keeps a pointer-only frame from
//!   rebuilding anything.
//!
//! The pointer, the pulse phase and the source text are deliberately absent from the
//! prepaint cache key: they change every frame, and everything they affect is drawn
//! by [`paint`] from the prepared frame.

pub(crate) mod canvas;
pub(crate) mod geometry;
pub(crate) mod hover;
pub(crate) mod paint;
pub(crate) mod text;

#[cfg(test)]
mod geometry_tests;

#[cfg(test)]
mod hover_tests;

#[cfg(test)]
mod paint_tests;

#[cfg(test)]
mod text_tests;
