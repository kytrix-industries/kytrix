//! The instrument and prediction-event selectors the panes open.
//!
//! Every module here is an application-layer dialog: a pane's control builds a node
//! subtree, the click opens a modal dialog through `base_ui::kit`, and the choice a
//! user confirms is handed back through the closure the caller passed. Nothing in
//! this crate reads a process global: the translation context, the theme, the frame
//! router, the layer stack and the market directory all arrive through
//! [`host::SelectorHost`], which the composition root assembles exactly the way it
//! assembles the services a widget is handed.
//!
//! ```text
//! pane (header control)                 dialog
//!   InstrumentButton::build(host, ..)      kit::dialog::Dialog
//!     on_click ──────────────────────────▶  content: tree + input + list
//!                                           confirm → on_confirm(selection, app, window)
//! ```
//!
//! # One frame hook, one owner
//!
//! The engine's frame hook is a single slot and kit components deliberately do not
//! register themselves, so an open dialog is synchronised by a route the selector
//! registers in the injected [`widget::frame::FrameRouter`]: `list`/`tree`/`input`
//! mount and measure, the dialog finishes its fade-out, the matcher's published
//! ranking is copied into the list, and the translation supervisor's results are
//! applied. The route removes itself when the dialog has closed, which is what lets
//! the state (and the kit components it owns) be released without a global holder.

pub mod host;
pub mod instrument_button;
pub mod instrument_selector;
pub mod prediction_button;
pub mod prediction_selector;
pub mod prediction_translation;
mod ui;

#[cfg(test)]
pub(crate) mod test_support;
