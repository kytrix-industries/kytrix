use std::{rc::Rc, sync::Arc};

use app_config::settings::{AccountConfig, Config};
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};
use futures::future::BoxFuture;
use i18n::{language::codes::LanguageCode, translate::Translate};
use kytrix_service_provider::{
    auth::{
        dpop::DpopKey,
        model::{AuthError, PlanCode, SessionChange},
        session::Revocation,
    },
    service::connector::KytrixServiceConnector,
};
use smol_str::SmolStr;
use tokio::sync::watch;
use ui_core::app::App;
use widget::{frame::FrameRouter, services::WidgetServices};
use zeroize::Zeroizing;

use crate::{oauth, session::LocalSession};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccountSnapshot {
    pub account: Option<AccountConfig>,
    pub avatar: Option<SmolStr>,
}

pub trait AccountSettings: 'static {
    fn read(&self, app: &App) -> AccountSnapshot;

    fn store_account(&self, app: &mut App, account: AccountConfig);

    fn clear_account(&self, app: &mut App);

    fn store_avatar_if_current(&self, app: &mut App, wallet: &str, svg: String) -> bool;
}

pub trait AccountService: Send + Sync + 'static {
    fn prepare_login(&self) -> Result<DpopKey, AuthError>;

    fn exchange_authorization_code<'a>(
        &'a self,
        dpop_key: DpopKey,
        code: String,
        redirect_uri: String,
        code_verifier: Zeroizing<String>,
    ) -> BoxFuture<'a, Result<AccountConfig, AuthError>>;

    fn discard_local_session(&self) -> Option<Revocation>;

    fn revoke_session<'a>(&'a self, revocation: Revocation) -> BoxFuture<'a, ()>;

    fn current_plan<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PlanCode>, AuthError>>;

    fn create_dashboard_handoff(&self) -> BoxFuture<'_, Result<String, AuthError>>;

    fn local_session(&self) -> BoxFuture<'_, Result<LocalSession, AuthError>>;

    fn session_changes(&self) -> watch::Receiver<SessionChange>;
}

impl AccountService for KytrixServiceConnector {
    fn prepare_login(&self) -> Result<DpopKey, AuthError> {
        self.prepare_account_login()
    }

    fn exchange_authorization_code<'a>(
        &'a self,
        dpop_key: DpopKey,
        code: String,
        redirect_uri: String,
        code_verifier: Zeroizing<String>,
    ) -> BoxFuture<'a, Result<AccountConfig, AuthError>> {
        Box::pin(oauth::exchange_authorization_code(
            self,
            dpop_key,
            code,
            redirect_uri,
            code_verifier,
        ))
    }

    fn discard_local_session(&self) -> Option<Revocation> {
        self.discard_account_session()
    }

    fn revoke_session<'a>(&'a self, revocation: Revocation) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.revoke_account_session(revocation).await })
    }

    fn current_plan<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PlanCode>, AuthError>> {
        Box::pin(async move { self.current_account_plan(user_id).await })
    }

    fn local_session(&self) -> BoxFuture<'_, Result<LocalSession, AuthError>> {
        Box::pin(async move {
            self.local_account_session_status()
                .await
                .map(LocalSession::from_status)
        })
    }

    fn create_dashboard_handoff(&self) -> BoxFuture<'_, Result<String, AuthError>> {
        Box::pin(self.create_account_dashboard_handoff())
    }

    fn session_changes(&self) -> watch::Receiver<SessionChange> {
        self.subscribe_account_session_changes()
    }
}

#[derive(Clone)]
pub struct ProfileHost {
    services: Rc<WidgetServices>,
    accounts: Arc<dyn AccountService>,
    settings: Rc<dyn AccountSettings>,
}

impl ProfileHost {
    #[must_use]
    pub fn new(
        services: Rc<WidgetServices>,
        accounts: Arc<dyn AccountService>,
        settings: Rc<dyn AccountSettings>,
    ) -> Self {
        Self {
            services,
            accounts,
            settings,
        }
    }

    #[must_use]
    pub fn services(&self) -> &WidgetServices {
        &self.services
    }

    #[must_use]
    pub fn services_rc(&self) -> Rc<WidgetServices> {
        Rc::clone(&self.services)
    }

    #[must_use]
    pub fn accounts(&self) -> &Arc<dyn AccountService> {
        &self.accounts
    }

    #[must_use]
    pub fn settings(&self) -> &Rc<dyn AccountSettings> {
        &self.settings
    }

    #[must_use]
    pub fn translate(&self) -> &Translate {
        &self.services.translate
    }

    #[must_use]
    pub fn theme(&self) -> &ThemeHandle {
        &self.services.theme
    }

    #[must_use]
    pub fn config_signal(&self) -> ui_core::node::signal::Signal<Config> {
        self.services.config
    }

    pub fn config_ref<R>(&self, app: &App, read: impl FnOnce(&Config) -> R) -> R {
        app.read_with(&self.services.config, read)
    }

    #[must_use]
    pub fn snapshot_signal(&self, app: &App) -> AccountSnapshot {
        self.config_ref(app, |config| AccountSnapshot {
            account: config.account.clone(),
            avatar: config.avatar.as_deref().map(SmolStr::new),
        })
    }

    #[must_use]
    pub fn language(&self, app: &App) -> LanguageCode {
        *app.read(&self.services.language)
    }

    #[must_use]
    pub fn language_signal(&self) -> ui_core::node::signal::Signal<LanguageCode> {
        self.services.language
    }

    #[must_use]
    pub fn reduce_motion(&self, app: &App) -> bool {
        *app.read(&self.services.reduce_motion)
    }

    pub fn open_url(&self, url: &str) {
        (self.services.open_url)(url);
    }

    #[must_use]
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.services.runtime
    }

    #[must_use]
    pub fn layer(&self) -> &Layer {
        &self.services.layer
    }

    #[must_use]
    pub fn router(&self) -> &FrameRouter {
        &self.services.frame_router
    }

    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        (*self.services.animation_router).clone()
    }
}
