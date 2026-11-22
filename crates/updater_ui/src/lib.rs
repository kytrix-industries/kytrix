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
//! updater::Updater::install(app, window, services, UpdaterDeps { transport, downloads, endpoint })
//!   └─ state::UpdaterState     the pure state machine (what a check offers, what a download
//!        ├─ fetch              does to it, what cancelling leaves behind)
//!        ├─ release            latest.json as typed values
//!        └─ install            the library's platform installer
//!   └─ indicator::UpdateIndicator   the top bar's button, while an update is offered
//!        └─ dialog::UpdateDialog    notes, progress, pause/resume, install
//! ```
//!
//! # Engine shape (P4-4)
//!
//! Nothing here reads a process global. The updater is assembled by the composition
//! root from the same [`WidgetServices`](widget::services::WidgetServices) aggregate a
//! widget is handed — its three composition-root singletons (layer stack, frame router,
//! animation router) are the ones the indicator and the dialog build through — plus the
//! two collaborators the toolkit used to supply: the application's transport, and a
//! [`DownloadService`](crate::download::DownloadService) over the process-wide download
//! manager (a seam, because that manager is still a toolkit entity until P5).
//!
//! The updater owns one frame route on the injected router. It drains the messages its
//! asynchronous work delivered, applies them to [`state::UpdaterState`], writes the
//! one signal the interface binds to ([`updater::UpdaterView`]) and synchronizes the
//! kit components. There is no render function and no rebuild-every-frame tree: the
//! indicator is a kit button, the dialog is a kit dialog whose content is rebuilt only
//! when the release it shows or the language it is drawn in changes.

mod dialog;
pub mod download;
pub mod error;
pub mod fetch;
pub mod indicator;
mod install;
mod publish;
pub mod release;
mod state;
pub mod updater;

#[cfg(test)]
mod test_support;
