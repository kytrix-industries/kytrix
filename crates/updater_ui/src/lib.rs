//! The application's own update path: check, download, install.
//!
//! The metadata request goes through the application's transport rather than the
//! updater library's own client, because it has to take the route the user
//! configured for it. The payload is a task of the process-wide download manager,
//! which is what makes it take the same route, be pausable and resumable, and
//! report its progress. Installation is the library's, so the three platform
//! installers are the ones the release packages are built against.
//!
//! ```text
//! global::install(cx, UpdaterDeps { transport, runtime, provider })
//!   └─ state::Updater      the state machine, the check and the install it runs
//!        ├─ fetch           the metadata request, route resolution and redirects
//!        ├─ release         latest.json as typed values
//!        └─ install         the library's platform installer
//!   └─ indicator::UpdateIndicator   the top bar's button, while an update is offered
//!        └─ dialog::UpdateDialog    notes, progress, pause/resume, install
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
