//! The tests of the settings page, split by the feature each one is about.
//!
//! The page's production code stays in `settings.rs`; its tests are here because the fixtures
//! they share are one window and one settings manager, and one file holding all of them would
//! be a file too large to read.

mod appearance;
mod layout;
mod proxy;
mod search;
mod session;
mod test_support;
mod translation;
mod translation_models;
mod venues;
