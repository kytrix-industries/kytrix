//! The application's download manager.
//!
//! A download is one HTTP transfer into one destination file. The crate owns the
//! resumable transfer itself — Range requests, validators, redirects, the
//! partial file and its replacement — the tasks it runs, the states they settle
//! in, and the typed vocabulary the interface draws for a download's progress
//! and failures.

pub mod error;
mod fetch;
pub mod global;
pub mod partial;
pub mod provider;
pub mod request;
pub mod task;
