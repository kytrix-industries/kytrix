//! The tests of the settings page, split by the feature each one is about.
//!
//! They are a sibling module rather than the page's own `tests` child because the workspace forbids
//! a `settings.rs` beside a `settings/` directory (`self_named_module_files`) and a
//! `settings/mod.rs` holding production definitions (`definition_in_module_root`). The page
//! exposes the few test-only accessors these tests need under `#[cfg(test)]`, so what production
//! code keeps private stays private.
//!
//! The fixtures they share are one window and one settings session; this module is where those are
//! built, so every test of the page is a test of the same arrangement.

mod appearance;
mod credentials;
mod downloads;
mod keys;
mod layout;
mod pages;
mod proxy;
mod search;
mod session;
mod session_page;
mod shell;
pub(crate) mod test_support;
mod translation;
mod translation_models;
mod venues;
