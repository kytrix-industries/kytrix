#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! The Kytrix application shell.
//!
//! The binary is the composition root and nothing else: it builds the one tokio runtime,
//! the one [`Composition`] (settings, translation, theme, the three application-level
//! registries, the widget catalogue and the services aggregate), opens the main window,
//! mounts the [`shell::Shell`] into it and runs the engine's event loop.
//!
//! # Startup order
//!
//! The order is a dependency order (see [`Composition::new`]): the settings are read
//! first because every later step projects from them, the translator renders the text the
//! rest publishes, the three registries exist before any component can register with
//! them, and the window is created last — a composition that failed has no window to
//! close. Nothing here installs a process global: the aggregate is handed down.
//!
//! # Frame ownership
//!
//! `App::on_frame` is a single slot. [`FrameRouter`] owns it (created exactly once, by
//! the composition root) and fans a tick out to the shell's route, the settings pages'
//! routes and every mounted widget's route; no module here calls `App::on_frame`, and
//! `App::on_animation_complete` is registered only by the composition root's
//! [`AnimationRouter`].

mod composition;
mod language;
mod shell;
#[cfg(test)]
mod test_support;
#[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
mod translation;

use app_config_provider::store::ConfigStore;
use thiserror::Error;
use ui_core::app::{App, Application};

use crate::composition::Composition;

fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("kytrix: failed to build the application runtime: {error}");
            std::process::exit(1);
        }
    };

    let mut app = App::new(runtime.handle().clone());
    let composition = match Composition::new(&mut app, runtime.handle().clone()) {
        Ok(composition) => composition,
        Err(error) => {
            log::error!("failed to initialize the application: {error}");
            eprintln!("kytrix: failed to initialize the application: {error}");
            std::process::exit(1);
        }
    };

    match shell::root::Shell::open(&mut app, std::rc::Rc::clone(&composition)) {
        Ok(shell) => {
            if let Err(error) = shell::root::Shell::install(&mut app, shell).map(|_| ()) {
                log::error!("failed to install the application shell: {error}");
                eprintln!("kytrix: failed to install the application shell: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            log::error!("failed to open the main window: {error}");
            eprintln!("kytrix: failed to open the main window: {error}");
            std::process::exit(1);
        }
    }

    if let Err(error) = Application::new().run(app) {
        log::error!("the application event loop failed: {error}");
        eprintln!("kytrix: the application event loop failed: {error}");
        std::process::exit(1);
    }
}

/// Failure to bring the application into a runnable state.
#[derive(Debug, Error)]
pub(crate) enum InitializationError {
    #[error(transparent)]
    Translations(#[from] i18n::error::types::Error),
    #[error(transparent)]
    Engine(#[from] ui_core::error::EngineError),
    #[error(transparent)]
    MarketParticipants(
        #[from] market_participants_connector::connector::MarketParticipantsConnectorError,
    ),
    #[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
    #[error(transparent)]
    AiTranslation(#[from] ai_translator::error::TranslationError),
    #[error("the widget catalogue could not be assembled: {0}")]
    Catalogue(#[from] widget_registry::registry::WidgetRegistryError),
}

/// The store the composition root opens before anything else exists.
///
/// Startup never fails on a damaged file: the store replaces it with defaults and reports
/// the recovery, which the shell announces to the user once the window is up.
pub(crate) fn open_config_store() -> (
    ConfigStore,
    Option<app_config_provider::store::ConfigRecovery>,
) {
    ConfigStore::load_default()
}

#[cfg(test)]
mod tests {
    use app_config::settings::Language;
    use i18n::language::codes::LanguageCode;
    use kytrix_service_provider::auth::constants::{ACCOUNT_ORIGIN, API_RESOURCE};

    use super::*;

    #[test]
    fn persisted_languages_should_map_to_the_translator_languages() {
        assert_eq!(crate::language::code(Language::ZhCn), LanguageCode::ZhCn);
        assert_eq!(crate::language::code(Language::En), LanguageCode::En);
    }

    /// The two halves of one switch: where this build sends requests, and whether the
    /// transport will carry them.
    ///
    /// They are decided in different crates — the origins by
    /// `kytrix_service_provider`'s `production` feature, the policy here — and a build
    /// that picks a plaintext origin while refusing plaintext cannot sign a user in.
    #[test]
    fn the_transport_policy_should_match_the_origins_the_build_uses() {
        let plaintext_origins =
            ACCOUNT_ORIGIN.starts_with("http://") && API_RESOURCE.starts_with("http://");

        assert_eq!(
            composition::transport_policy().allow_plaintext,
            plaintext_origins,
            "account origin {ACCOUNT_ORIGIN}, resource {API_RESOURCE}"
        );
    }
}
