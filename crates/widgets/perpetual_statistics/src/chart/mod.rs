mod axes;
pub(crate) mod canvas;
pub(crate) mod open_interest;
mod paint;
mod palette;
mod ratio;
mod rect;
pub(crate) mod taker_volume;
mod text;

pub(crate) use canvas::ChartDispatch;

#[cfg(test)]
mod axes_tests;

#[cfg(test)]
mod canvas_tests;

#[cfg(test)]
mod paint_tests;

#[cfg(test)]
mod text_tests;
