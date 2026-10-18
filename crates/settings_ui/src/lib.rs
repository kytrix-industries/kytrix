//! The settings dialog: everything the application is configured by, in one window.
//!
//! [`entry::show_settings`] presents the dialog. The panel it draws has a tree of the settings
//! on one side and the page behind the selected entry on the other; the form that changes the
//! proxy catalog is a dialog of its own on the same window layer.
//!
//! The settings themselves are not owned here, but neither does a control write them. They
//! live in `app_config_provider`'s global, a session reads them once when it opens, and every
//! control changes the session instead — see [`draft`]. The dialog's confirm button is what
//! writes the session into the settings, and the manager persists it; settings that could not
//! be stored are reported to the user, and what they changed is still there to try again.

mod access;
mod catalog;
mod dialog;
mod draft;
mod editor;
mod page;
mod schema;
mod search;
mod table;

pub mod appearance;
pub mod entry;
