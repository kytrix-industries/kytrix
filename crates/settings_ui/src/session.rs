//! What one settings session writes: every change is stored as it is made.
//!
//! Nothing here holds the settings. They are the application's configuration snapshot, published
//! as a signal by the composition root, and a change is applied to the snapshot of the moment it is
//! written rather than to a copy taken when the page opened: background work writes them while the
//! page is open — an avatar is generated, a workspace layout is persisted, a sidebar state is
//! saved — and a write must not undo it.
//!
//! A change is stored before it is published: the commit seam the host carries persists the
//! candidate and only then publishes it, so a change that cannot be stored leaves the settings
//! exactly as they were. The reason is kept here for the page to report, and what the user typed is
//! what they can try again with.
//!
//! # Why the store is a value, not an entity
//!
//! The engine has no entity system: what a page observes is a [`Signal`], and what it writes
//! through is an injected closure. [`SettingsStore`] is that pair, so the same value is what the
//! composition root hands the page in production and what a test builds over a snapshot it can
//! fail on purpose.

use std::{cell::RefCell, error::Error, fmt, path::PathBuf, rc::Rc, sync::Arc};

use app_config::{
    network::ProxyConfig,
    settings::Config,
    translation::{DownloadSite, TranslationModel},
};
use app_config_provider::store::ConfigPersistenceError;
use smol_str::SmolStr;
use ui_core::{app::App, node::signal::Signal, window::WindowId};

/// Why a change could not be stored.
///
/// The store's own error is shared rather than copied, because the page keeps the last one until
/// it has reported it and a [`ConfigPersistenceError`] is not cloneable.
#[derive(Clone, Debug)]
pub struct SettingsWriteError {
    cause: Arc<ConfigPersistenceError>,
}

impl SettingsWriteError {
    /// Wraps the store's failure.
    #[must_use]
    pub fn new(cause: ConfigPersistenceError) -> Self {
        Self {
            cause: Arc::new(cause),
        }
    }
}

impl fmt::Display for SettingsWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.cause)
    }
}

impl Error for SettingsWriteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

/// One change a native setting makes to the configuration.
///
/// The change is a closure rather than a variant of its own: a setting page writes the same typed
/// values it reads, and the closure carries whatever identity it needs — a venue, a market, a
/// credential name — without the session having to know the shape of every page.
pub struct ConfigEdit(Rc<dyn Fn(&mut Config)>);

impl ConfigEdit {
    pub(crate) fn new(apply: impl Fn(&mut Config) + 'static) -> Self {
        Self(Rc::new(apply))
    }

    fn apply(&self, config: &mut Config) {
        (self.0)(config);
    }
}

/// One change made to the settings, applied to the snapshot it is written on.
pub enum SettingsChange {
    /// One native setting, written the way its page writes it.
    Edit(ConfigEdit),
    /// The directory the local translation model is loaded from.
    TranslationModelDirectory { directory: PathBuf },
    /// The model the local translation is loaded from, or none when no model is chosen.
    TranslationModelSelected { model: Option<TranslationModel> },
    /// The site the local translation model is downloaded from.
    TranslationDownloadSite { site: DownloadSite },
    /// One entry added to the proxy catalog.
    AddProxy { name: SmolStr, proxy: ProxyConfig },
    /// One entry renamed and rewritten, together with every route bound to its name.
    ReplaceProxy {
        original: SmolStr,
        name: SmolStr,
        proxy: ProxyConfig,
    },
    /// One entry removed, together with every route bound to its name.
    RemoveProxy { name: SmolStr },
}

impl SettingsChange {
    /// One native setting's change.
    pub(crate) fn edit(apply: impl Fn(&mut Config) + 'static) -> Self {
        Self::Edit(ConfigEdit::new(apply))
    }

    /// Makes this change in `config`.
    pub(crate) fn apply(&self, config: &mut Config) {
        match self {
            Self::Edit(edit) => edit.apply(config),
            Self::TranslationModelDirectory { directory } => {
                config.translation.model_directory = Some(directory.clone());
            }
            Self::TranslationModelSelected { model } => {
                config.translation.selected_model = *model;
            }
            Self::TranslationDownloadSite { site } => {
                config.translation.download_site = *site;
            }
            Self::AddProxy { name, proxy } => {
                config.proxies.insert(name.clone(), proxy.clone());
            }
            Self::ReplaceProxy {
                original,
                name,
                proxy,
            } => {
                // The form is modal and is the only interface that writes the catalog, so a
                // replacement that cannot be made means the settings were changed by hand
                // outside the application: the source entry is gone or its new name is taken.
                // The failure is logged and the change is abandoned rather than written as an
                // insert, which would leave every route bound to the old name.
                if let Err(error) = config.replace_proxy(original, name.clone(), proxy.clone()) {
                    log::error!("failed to replace proxy {original}: {error}");
                }
            }
            Self::RemoveProxy { name } => {
                config.remove_proxy(name);
            }
        }
    }
}

/// The commit seam every write of the settings page goes through.
///
/// The composition root implements it over its store: it persists `candidate` and, only when that
/// succeeded, publishes it as the configuration snapshot the settings signal carries (and lets the
/// window layers that follow the settings know). A headless test implements it over a snapshot it
/// owns, which is what lets a test make a write fail without a filesystem behind it.
pub type Commit = Rc<dyn Fn(&mut App, Config) -> Result<(), SettingsWriteError>>;

/// The settings one page reads and writes through.
///
/// Cheap to clone (one `Rc`), because a control's callback and the page that drew it share the
/// same session.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Rc<StoreInner>,
}

/// The one state a store shares with every control that writes through it.
struct StoreInner {
    config: Signal<Config>,
    commit: Commit,
    /// Why the last write could not be stored, waiting for the page to take it and report it.
    failure: RefCell<Option<SettingsWriteError>>,
}

impl SettingsStore {
    /// The store over the snapshot `config` and the application's commit seam.
    #[must_use]
    pub fn new(
        app: &mut App,
        config: Signal<Config>,
        commit: impl Fn(&mut App, Config) -> Result<(), SettingsWriteError> + 'static,
    ) -> Self {
        let _ = app;
        Self {
            inner: Rc::new(StoreInner {
                config,
                commit: Rc::new(commit),
                failure: RefCell::new(None),
            }),
        }
    }

    /// The settings snapshot the page draws from.
    #[must_use]
    pub fn config_signal(&self) -> Signal<Config> {
        self.inner.config
    }

    /// Reads one value out of the settings of the moment, without cloning the snapshot.
    pub fn read<R>(&self, app: &App, read: impl FnOnce(&Config) -> R) -> R {
        app.read_with(&self.inner.config, read)
    }

    /// Writes one change into the settings.
    ///
    /// # Errors
    /// The commit seam refused to store the candidate (the settings are then as they were).
    pub fn write(
        &self,
        app: &mut App,
        window: WindowId,
        change: SettingsChange,
    ) -> Result<(), SettingsWriteError> {
        self.write_all(app, window, std::iter::once(change))
    }

    /// Writes a group of changes into the settings as one stored configuration.
    ///
    /// The order is the order they are applied in. A directory and the selection it invalidates go
    /// through here, so the two land in one write and one set of events: there is no moment where
    /// the stored directory moved but the selection still names the old one.
    ///
    /// # Errors
    /// The commit seam refused to store the candidate (the settings are then as they were).
    pub fn write_all(
        &self,
        app: &mut App,
        window: WindowId,
        changes: impl IntoIterator<Item = SettingsChange>,
    ) -> Result<(), SettingsWriteError> {
        let changes = changes.into_iter().collect::<Vec<_>>();
        if changes.is_empty() {
            return Ok(());
        }

        let current = self.read(app, Config::clone);
        let mut candidate = current.clone();
        for change in changes {
            change.apply(&mut candidate);
        }
        // A change that produces what is stored already is not a write: the settings keep the
        // value they have, and there is nothing for the user to be told.
        if candidate == current {
            // A control that offered a value the settings refused draws the old value again on the
            // frame that follows, so the frame is asked for even though nothing was stored.
            app.request_repaint(window).ok();
            return Ok(());
        }

        let result = (self.inner.commit)(app, candidate);
        if let Err(error) = &result {
            *self.inner.failure.borrow_mut() = Some(error.clone());
        }
        // What draws the settings is told the settings moved; a failed write repaints too, because
        // the page is what takes the reason and reports it.
        app.request_repaint(window).ok();
        result
    }

    /// Takes the last failure, so the page reports one reason once.
    pub fn take_failure(&self) -> Option<SettingsWriteError> {
        self.inner.failure.borrow_mut().take()
    }

    /// Whether a failure is waiting to be reported.
    #[must_use]
    pub fn has_failure(&self) -> bool {
        self.inner.failure.borrow().is_some()
    }
}
