#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod windows;

use std::sync::Arc;

use app_config::settings::Language;
use app_config_provider::{
    manager::{self as config_manager, ConfigManager},
    store::ConfigRecovery,
};
use i18n::language::codes::LanguageCode;
use kytrix_service_provider::{
    auth::{model::AuthError, session::AuthSession},
    service::connector::KytrixServiceConnector,
};
use market_participants_connector::connector::{
    MarketParticipantsConnector, MarketParticipantsConnectorError,
};
use network::transport::hub::{TransportHub, TransportPolicy};
use thiserror::Error;
use updater_ui::global::UpdaterDeps;

fn main() {
    gpui_kit::application()
        .with_assets(base_ui::assets::Assets)
        .run(|cx| match initialize(cx) {
            Ok(recovery) => windows::root::window::open(cx, recovery),
            Err(error) => {
                log::error!("failed to initialize the application: {error}");
                cx.quit();
            }
        });
}

/// Installs every global the UI reads before the first window opens.
///
/// The order is a dependency order: the translator renders the text every later
/// step publishes, the shared runtime drives the services built from the
/// persisted settings, and then the market participants root is built — it owns the
/// catalogue, the venues and the runtime projection itself. Installing it is
/// wiring and nothing else: no venue is loaded at startup, because the only
/// action that loads a market is the user's own, through the market-participants
/// menu. The updater is installed last, on the transport the service connection
/// was built with: nothing else reads it, and its first check is a delay rather
/// than work this call does.
///
/// Returns the settings recovery, if the stored file was unusable and had to be
/// replaced with defaults. Starting up is never blocked by a damaged file, and the
/// window is what tells the user about it.
fn initialize(cx: &mut gpui_kit::App) -> Result<Option<ConfigRecovery>, InitializationError> {
    gpui_kit::base::init(cx);
    base_ui::style::theme::install(cx);
    i18n::global::embedded::install(cx)?;
    widget_tokio_runtime::global::install(cx)?;
    widget_registry::catalogue::install(cx)?;
    let recovery = config_manager::install(cx);
    apply_startup_language(cx);
    MarketParticipantsConnector::install(cx)?;
    let transport = install_kytrix_service(cx)?;
    let runtime = widget_tokio_runtime::global::try_handle(cx)
        .ok_or(InitializationError::RuntimeUnavailable)?;
    let config = cx
        .try_global::<ConfigManager>()
        .ok_or(InitializationError::ConfigurationUnavailable)?
        .network_subscribe();
    updater_ui::global::install(
        cx,
        UpdaterDeps {
            transport,
            config,
            runtime,
        },
    );

    Ok(recovery)
}

/// Connects the application to the Kytrix service.
///
/// Every Kytrix-backed widget reads this one connector, and it is built here
/// rather than lazily because the login it carries is a property of the process:
/// the account a user signed in as is what those widgets are scoped to. Building
/// it does not fetch anything — a request is what opens the connection.
///
/// The transport comes back because it is the process's, not the connector's: the
/// updater takes the same one, so an update request leaves along the route the user
/// configured for it, under the same lifecycle barrier as everything else.
fn install_kytrix_service(
    cx: &mut gpui_kit::App,
) -> Result<Arc<TransportHub>, InitializationError> {
    let runtime = widget_tokio_runtime::global::try_handle(cx)
        .ok_or(InitializationError::RuntimeUnavailable)?;
    let manager = cx
        .try_global::<ConfigManager>()
        .ok_or(InitializationError::ConfigurationUnavailable)?;
    let transport = Arc::new(TransportHub::new(transport_policy()));
    let auth = AuthSession::spawn(
        manager.network_subscribe(),
        transport.clone(),
        runtime.clone(),
    )?;
    cx.set_global(KytrixServiceConnector::spawn(
        &runtime,
        manager.network_subscribe(),
        transport.clone(),
        auth,
    ));

    Ok(transport)
}

/// The transport policy the build calls for.
///
/// A development build talks to the service on the loopback, which is plaintext: the
/// account origin is `http://127.0.0.1:18010` and the service socket is `ws://`, so a
/// TLS-only transport would refuse every request — including the token exchange that
/// finishes a sign-in. A production build talks to `https://account.kytrix.xyz` and
/// `wss://api.kytrix.xyz` and never sends plaintext anywhere.
///
/// The policy is derived from the same `production` switch as those origins, so the two
/// cannot disagree about which environment this build is for.
const fn transport_policy() -> TransportPolicy {
    TransportPolicy {
        allow_plaintext: !cfg!(feature = "production"),
        extra_root_certificates: Vec::new(),
    }
}

/// Applies the persisted display language to the language global.
fn apply_startup_language(cx: &mut gpui_kit::App) {
    let language = cx.global::<ConfigManager>().read().language;

    cx.set_global(language_code(language));
}

const fn language_code(language: Language) -> LanguageCode {
    match language {
        Language::ZhCn => LanguageCode::ZhCn,
        Language::En => LanguageCode::En,
    }
}

/// Failure to bring the application into a runnable state.
#[derive(Debug, Error)]
enum InitializationError {
    #[error(transparent)]
    Translations(#[from] i18n::error::types::Error),
    #[error(transparent)]
    TokioRuntime(#[from] widget_tokio_runtime::global::TokioRuntimeError),
    #[error(transparent)]
    WidgetRegistry(#[from] widget_registry::registry::WidgetRegistryError),
    #[error(transparent)]
    MarketParticipants(#[from] MarketParticipantsConnectorError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("a Tokio runtime is required for the Kytrix service connection")]
    RuntimeUnavailable,
    #[error("the application settings are not installed")]
    ConfigurationUnavailable,
}

#[cfg(test)]
mod tests {
    use kytrix_service_provider::auth::constants::{ACCOUNT_ORIGIN, API_RESOURCE};

    use super::*;

    #[test]
    fn persisted_languages_should_map_to_the_translator_languages() {
        assert_eq!(language_code(Language::ZhCn), LanguageCode::ZhCn);
        assert_eq!(language_code(Language::En), LanguageCode::En);
    }

    /// The two halves of one switch: where this build sends requests, and whether the
    /// transport will carry them.
    ///
    /// They are decided in different crates — the origins by
    /// `kytrix_service_provider`'s `production` feature, the policy here — and a build
    /// that picks a plaintext origin while refusing plaintext cannot sign a user in:
    /// the browser authorizes, the callback arrives, and the token exchange is refused
    /// by the TLS-only client. That is exactly what this test exists to prevent.
    #[test]
    fn the_transport_policy_should_match_the_origins_the_build_uses() {
        let plaintext_origins =
            ACCOUNT_ORIGIN.starts_with("http://") && API_RESOURCE.starts_with("http://");

        assert_eq!(
            transport_policy().allow_plaintext,
            plaintext_origins,
            "account origin {ACCOUNT_ORIGIN}, resource {API_RESOURCE}"
        );
    }
}
