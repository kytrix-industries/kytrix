#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! The Kytrix application shell.
//!
//! The binary is the composition root and nothing else: it builds the one tokio runtime,
//! the one [`Composition`] (settings, translation, theme, the three application-level
//! registries, the widget catalogue, the services aggregate, the account connection and the
//! download manager), opens the main window, mounts the [`shell::Shell`] into it and runs
//! the engine's event loop.
//!
//! # Startup order
//!
//! The order is a dependency order (see [`Composition::new`]): the settings are read
//! first because every later step projects from them, the translator renders the text the
//! rest publishes, the three registries exist before any component can register with
//! them, and the window is created last — a composition that failed has no window to
//! close. Nothing here installs a process global: the aggregate is handed down.
//!
//! # Logging
//!
//! The log sink is installed before the runtime is built: everything below this point —
//! including a start that fails — reports through `log` to the one implementation. The
//! filter is `RUST_LOG`, defaulting to `info`.
//!
//! # Frame ownership
//!
//! `App::on_frame` is a single slot. [`FrameRouter`] owns it (created exactly once, by
//! the composition root) and fans a tick out to the shell's route, the settings pages'
//! routes and every mounted widget's route; no module here calls `App::on_frame`, and
//! `App::on_animation_complete` is registered only by the composition root's
//! [`AnimationRouter`].

#[cfg(any(test, feature = "acceptance"))]
mod acceptance;
mod account;
mod composition;
mod downloads;
mod language;
mod shell;
#[cfg(test)]
mod test_support;
#[cfg(ai_translation)]
mod translation;

use app_config_provider::store::ConfigStore;
use thiserror::Error;
use ui_core::app::{App, Application};

use crate::composition::Composition;

fn main() {
    install_logging();

    #[cfg(feature = "acceptance")]
    let acceptance = acceptance::options::Options::parse(std::env::args().skip(1));

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
    // The acceptance run drives a real window, but over a fresh in-memory installation: it
    // switches the language and the appearance as part of its steps and must not touch the
    // user's settings file or the keyring. A normal start (including a full-featured build
    // that happens to contain the acceptance code) takes the production path.
    #[cfg(feature = "acceptance")]
    let composition = if acceptance.is_some() {
        Composition::for_acceptance(&mut app, runtime.handle().clone())
    } else {
        Composition::new(&mut app, runtime.handle().clone())
    };
    #[cfg(not(feature = "acceptance"))]
    let composition = Composition::new(&mut app, runtime.handle().clone());
    let composition = match composition {
        Ok(composition) => composition,
        Err(error) => {
            log::error!("failed to initialize the application: {error}");
            eprintln!("kytrix: failed to initialize the application: {error}");
            std::process::exit(1);
        }
    };

    let shell = match shell::root::Shell::open(&mut app, std::rc::Rc::clone(&composition)) {
        Ok(shell) => shell,
        Err(error) => {
            log::error!("failed to open the main window: {error}");
            eprintln!("kytrix: failed to open the main window: {error}");
            std::process::exit(1);
        }
    };
    // The acceptance run drives the window the shell draws; a normal start never reads it.
    #[cfg(feature = "acceptance")]
    let acceptance_window = shell.window_id();
    if let Err(error) = shell::root::Shell::install(&mut app, shell).map(|_| ()) {
        log::error!("failed to install the application shell: {error}");
        eprintln!("kytrix: failed to install the application shell: {error}");
        std::process::exit(1);
    }

    // The acceptance run drives the window from its own E10 timer chain; a normal start has
    // no such clock.
    #[cfg(feature = "acceptance")]
    let acceptance_run = acceptance
        .map(|options| acceptance::host::start(&mut app, &composition, acceptance_window, options));

    let result = Application::new().run(app);

    #[cfg(feature = "acceptance")]
    if let Some(run) = acceptance_run {
        if let Err(error) = &result {
            run.record_engine_error(error);
        }
        std::process::exit(if run.finish() { 0 } else { 1 });
    }

    if let Err(error) = result {
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
    Auth(#[from] kytrix_service_provider::auth::model::AuthError),
    #[cfg(ai_translation)]
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

/// Installs the application's one log sink.
///
/// Everything below this call reports through `log`; without an implementation those
/// records go nowhere (the shell migrated off the toolkit, which used to install one). The
/// filter is `RUST_LOG`, defaulting to `info`; the stream is stderr, which is what the
/// terminal and the platform's crash reports capture.
fn install_logging() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
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
