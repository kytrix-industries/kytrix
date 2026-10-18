//! The application's own update path: check, download, install.
//!
//! The requests go through the application's transport rather than the updater
//! library's own client, because they have to take the route the user configured
//! for them, be cancellable, and report progress. Installation is the library's,
//! so the three platform installers are the ones the release packages are built
//! against.
//!
//! ```text
//! global::install(cx, UpdaterDeps { transport, config, runtime })
//!   └─ state::Updater      the state machine and the requests it runs
//!        ├─ fetch           route resolution, redirects, progress
//!        ├─ release         latest.json as typed values
//!        └─ install         the library's platform installer
//!   └─ indicator::UpdateIndicator   the top bar's button, while an update is offered
//!        └─ dialog::UpdateDialog    notes, progress, install
//! ```

mod dialog;
pub mod error;
pub mod fetch;
pub mod global;
pub mod indicator;
mod install;
mod publish;
pub mod release;
mod state;
