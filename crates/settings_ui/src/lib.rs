//! The settings page: everything the application is configured by, in one view.
//!
//! [`settings::SettingsPage`] is the page the main window draws in place of the workspace while
//! the user configures the application. It has a tree of the settings on one side and the page
//! behind the selected entry on the other; the form that changes the proxy catalog is a dialog of
//! its own on the window layer.
//!
//! The settings themselves are not owned here, but neither does a control write them. They
//! live in `app_config_provider`'s global, a session reads them once when it opens, and every
//! control changes the session instead — see [`draft`]. The page's confirm button is what
//! writes the session into the settings, and the manager persists it; settings that could not
//! be stored are reported to the user, and what they changed is still there to try again.

mod access;
mod catalog;
mod draft;
mod editor;
mod page;
mod schema;
mod search;
pub mod settings;
mod table;
mod translation;
pub mod translation_downloads;
mod translation_site;
mod translation_table;

pub mod appearance;
