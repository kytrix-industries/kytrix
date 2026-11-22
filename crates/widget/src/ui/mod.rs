//! The elements a widget body is made of.
//!
//! These live with the framework rather than with the shared component library
//! because only the framework decides when they are shown. What they are built
//! from — spinners, themes, buttons — stays in `base_ui::kit`.

pub mod placeholder;
