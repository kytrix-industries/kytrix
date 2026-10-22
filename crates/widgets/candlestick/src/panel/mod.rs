//! The indicator settings panel: the configured indicators and the controls that edit them.

mod inputs;
mod rows;
mod view;

pub(crate) use view::{IndicatorPanel, PANEL_WIDTH};

#[cfg(test)]
mod view_tests;
