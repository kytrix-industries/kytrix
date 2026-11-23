use std::{cell::RefCell, error::Error, fmt, path::PathBuf, rc::Rc, sync::Arc};

use app_config::{
    network::ProxyConfig,
    settings::Config,
    translation::{DownloadSite, TranslationModel},
};
use app_config_provider::store::ConfigPersistenceError;
use smol_str::SmolStr;
use ui_core::{app::App, node::signal::Signal, window::WindowId};

#[derive(Clone, Debug)]
pub struct SettingsWriteError {
    cause: Arc<ConfigPersistenceError>,
}

impl SettingsWriteError {
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

pub struct ConfigEdit(Rc<dyn Fn(&mut Config)>);

impl ConfigEdit {
    pub(crate) fn new(apply: impl Fn(&mut Config) + 'static) -> Self {
        Self(Rc::new(apply))
    }

    fn apply(&self, config: &mut Config) {
        (self.0)(config);
    }
}

pub enum SettingsChange {
    Edit(ConfigEdit),
    TranslationModelDirectory {
        directory: PathBuf,
    },
    TranslationModelSelected {
        model: Option<TranslationModel>,
    },
    TranslationDownloadSite {
        site: DownloadSite,
    },
    AddProxy {
        name: SmolStr,
        proxy: ProxyConfig,
    },
    ReplaceProxy {
        original: SmolStr,
        name: SmolStr,
        proxy: ProxyConfig,
    },
    RemoveProxy {
        name: SmolStr,
    },
}

impl SettingsChange {
    pub(crate) fn edit(apply: impl Fn(&mut Config) + 'static) -> Self {
        Self::Edit(ConfigEdit::new(apply))
    }

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

pub type Commit = Rc<dyn Fn(&mut App, Config) -> Result<(), SettingsWriteError>>;

#[derive(Clone)]
pub struct SettingsStore {
    inner: Rc<StoreInner>,
}

struct StoreInner {
    config: Signal<Config>,
    commit: Commit,
    failure: RefCell<Option<SettingsWriteError>>,
}

impl SettingsStore {
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

    #[must_use]
    pub fn config_signal(&self) -> Signal<Config> {
        self.inner.config
    }

    pub fn read<R>(&self, app: &App, read: impl FnOnce(&Config) -> R) -> R {
        app.read_with(&self.inner.config, read)
    }

    pub fn write(
        &self,
        app: &mut App,
        window: WindowId,
        change: SettingsChange,
    ) -> Result<(), SettingsWriteError> {
        self.write_all(app, window, std::iter::once(change))
    }

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
        if candidate == current {
            app.request_repaint(window).ok();
            return Ok(());
        }

        let result = (self.inner.commit)(app, candidate);
        if let Err(error) = &result {
            *self.inner.failure.borrow_mut() = Some(error.clone());
        }
        app.request_repaint(window).ok();
        result
    }

    pub fn take_failure(&self) -> Option<SettingsWriteError> {
        self.inner.failure.borrow_mut().take()
    }

    #[must_use]
    pub fn has_failure(&self) -> bool {
        self.inner.failure.borrow().is_some()
    }
}
