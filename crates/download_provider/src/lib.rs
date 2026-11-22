//! The application's download manager.
//!
//! A download is one HTTP transfer into one destination file. The crate owns the
//! resumable transfer itself — Range requests, validators, redirects, the
//! partial file and its replacement — the tasks it runs, the states they settle
//! in, and the typed vocabulary the interface draws for a download's progress
//! and failures.
//!
//! [`provider::DownloadProvider`] is the manager: the composition root owns it,
//! every transfer runs on the application runtime it is handed, and the one
//! observer that needs the event stream subscribes to it. Nothing here reads a
//! process global, and nothing here needs an interface host to exist.

pub mod error;
mod fetch;
pub mod partial;
mod progress;
pub mod provider;
pub mod request;
pub mod task;
