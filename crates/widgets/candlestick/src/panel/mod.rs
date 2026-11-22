//! The indicator settings panel: the configured indicators and the controls that edit them.

mod colors;
mod inputs;
mod rows;
mod view;

pub(crate) use view::{IndicatorPanel, PANEL_WIDTH, PanelRequest};

#[cfg(test)]
mod smoke_tests;
