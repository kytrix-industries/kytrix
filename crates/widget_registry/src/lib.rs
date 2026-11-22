//! The statically linked widget catalogue and its registry.
//!
//! Every widget is compiled into the binary, so the complete catalogue is known at
//! build time and the application never registers widgets dynamically. [`catalogue`]
//! assembles the table of all 24 entries and [`registry`] is the erased mount a pane
//! looks up by the ID it restored, on the engine contract (`AnyWidget`, `WidgetStatus`,
//! `HeaderGroup`) with the composition root's `Rc<WidgetServices>` passed in. Nothing
//! here is a process global: the composition root builds the registry once and keeps it.

pub mod catalogue;
pub mod registry;
