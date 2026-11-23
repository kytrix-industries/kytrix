mod axes;
mod canvas;
mod data;
mod paint;
mod palette;
mod text;
mod units;

pub(crate) use canvas::CryptocurrencyEtfChart;
pub(crate) use data::Metrics;

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
