//! The settings page: everything the application is configured by, in one view.
//!
//! [`settings::SettingsPage`] is the page the main window draws in place of the workspace while
//! the user configures the application. It has a tree of the settings on one side and the page
//! behind the selected entry on the other; the form that changes the proxy catalog and the dialog
//! that edits a market participant's trading credentials are dialogs of their own on the window
//! layer.
//!
//! The settings themselves are not owned here: they live in `app_config_provider`'s global, and
//! every control writes the change it makes at once — see [`session`]. A change is applied to the
//! settings as they are at that moment and stored before it is published, so a change that cannot
//! be stored leaves the settings as they were and is reported to the user. There is nothing to
//! confirm and nothing to cancel on the page itself: leaving it keeps what was stored. A dialog is
//! the one place that holds a draft, and only the dialog writing it stores it.
//!
//! Every page is native UI drawn by the module that owns it. [`pages`] holds the settings pages —
//! the ones that are a list of setting rows, the credentials of a venue, the proxy catalog and its
//! form, and the translation page — [`pages::registry`] is the navigation and search index over all
//! of them, and [`layout`] is the row and card the pages are built from.

pub mod appearance;
mod layout;
mod pages;
mod search;
mod session;
pub mod settings;
#[cfg(test)]
mod settings_tests;
pub mod translation_downloads;
