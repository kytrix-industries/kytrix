//! The collaborators the account control is handed instead of reading process globals.
//!
//! The control is an application-layer component the title bar mounts, so the three things it
//! cannot own are assembled by the composition root and passed in:
//!
//! - [`WidgetServices`] — the translation context, the language signal, the settings snapshot,
//!   the theme, the reduced-motion preference, the open-url command **and the three
//!   application-level registries kit components require** (the layer stack, the frame router
//!   and the animation router), plus the tokio runtime. It is the aggregate a widget is handed,
//!   frozen in P3-1d/P3-1e; a title-bar control is not a widget, but it is assembled from
//!   exactly the same services, so a shell that has a [`WidgetCx`](widget::cx::WidgetCx) already
//!   has everything a host needs ([`WidgetCx::services_rc`](widget::cx::WidgetCx::services_rc)).
//! - [`AccountService`] — the account service's OAuth session: prepare a sign-in, exchange the
//!   browser's authorization code, read the local credential, query the tier, hand the
//!   dashboard a one-time code, sign out, and watch for the session ending. It is a seam, not
//!   an abstraction: the control is tested against a fake so a rejected refresh, an account
//!   that switched mid-query and a sign-out that failed can each be produced on demand, and the
//!   production implementation is the connector ([`KytrixServiceConnector`]) with three
//!   delegating methods and one conversion.
//! - [`AccountSettings`] — the store that *owns* the account record. The control reads it (the
//!   record and the generated avatar), writes it when a sign-in completes or a sign-out is
//!   confirmed, and drops the avatar when the record changes. The concrete store is the
//!   composition root's (P4-3); this crate must not grow a bridge to one.
//!
//! The host is `Clone` and carries only an `Rc<WidgetServices>` plus two shared handles, so a
//! control keeps the services it was built with even after the caller drops its own copy, and
//! nothing here is copied per control.
//!
//! # Assembling one
//!
//! The composition root builds the services once (the same aggregate the widgets are handed)
//! and wraps it. The account service is the connector it already installed; the settings store
//! is the root's own, exposed behind the trait below.
//!
//! ```text
//! // Composition root, once — the same builder a widget host uses:
//! let services = Rc::new(
//!     WidgetServices::builder(app, runtime, translate, theme, layer, frame_router, animation_router)
//!         .language(language)
//!         .config(config)
//!         .open_url(|url| /* the shell's browser command */)
//!         .build(),
//! );
//! let host = ProfileHost::new(Rc::clone(&services), accounts, settings);
//!
//! // The title bar mounts the control and drives its frames:
//! let mut menu = ProfileMenu::new(host)?;
//! menu.build(app, window, slot)?;
//! // …once per frame, from the shell's frame route:
//! menu.sync(app)?;
//! ```
//!
//! # Why the settings are a seam of their own
//!
//! The account record is the one piece of state the control both reads and writes, and it
//! belongs to the application settings — a settings manager the composition root owns, whose
//! concrete type (and whose change event) is the root's business, not a dialog's. Injecting the
//! four operations the control actually performs keeps the write rules in one place (see
//! [`AccountSettings`]) and keeps this crate from reaching for a process global.
//!
//! # Reactivity
//!
//! The root keeps [`WidgetServices::config`] in step with its settings manager (that is the
//! frozen contract, `crates/widgets/AGENTS.md` §8.1). The control therefore reads that signal
//! in [`ProfileMenu::sync`](crate::profile::ProfileMenu::sync) and compares the two fields it
//! cares about against the snapshot it last applied: a settings write is what makes those two
//! fields change, so a write that changes neither (a proxy, a theme) costs no re-read, and a
//! write that changes the account or its avatar re-reads the authoritative snapshot through
//! [`AccountSettings::read`]. `bind_*` closures inside the menu read the same signal directly
//! when they need a live value (the address line, the tier).

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

/// What the account settings hold for this feature: who is signed in, and the generated
/// avatar of that account.
///
/// It is a snapshot rather than a `&Config` because the control only ever wants these two
/// fields, and comparing a snapshot is how it notices the account changing (see the module
/// documentation on reactivity). The avatar travels with the account because it belongs to it:
/// an avatar without an account is a face for nobody and is not readable
/// ([`crate::account::avatar_of`]).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccountSnapshot {
    /// The account the application is signed in as, when it is signed in.
    pub account: Option<AccountConfig>,
    /// The SVG of the generated avatar of that account, once one has been generated.
    pub avatar: Option<SmolStr>,
}

/// The application settings as this feature reads and writes them.
///
/// The store is the composition root's (P4-3): it is the settings manager that also feeds
/// [`WidgetServices::config`], and this trait is the four operations the control performs on
/// it. The rules are part of the contract, and each is a test of this crate:
///
/// 1. [`read`](Self::read) reports the record and the avatar of that record. An avatar stored
///    without an account is not part of a snapshot — [`crate::account::avatar_of`] states the
///    same rule on the value, so a reader cannot see a face for nobody even if the store kept
///    one.
/// 2. [`store_account`](Self::store_account) replaces the record **and drops the avatar in the
///    same write**: the face belongs to the account that just went away, and a menu that showed
///    the new address beside the old face for even one frame would be showing two accounts.
/// 3. [`clear_account`](Self::clear_account) removes the record and the avatar with it.
/// 4. [`store_avatar_if_current`](Self::store_avatar_if_current) writes an avatar **only** if the
///    wallet it was generated for is still the signed-in wallet, and reports whether it did.
///    Generation is a background task, so it can finish after the user signed out or after a
///    different wallet signed in; writing it then would put one account's face on another.
///
/// Every write must also make the root republish [`WidgetServices::config`], which is what asks
/// the control for the frame that reads the new record (the same signal the widgets follow).
pub trait AccountSettings: 'static {
    /// The account record and its avatar as they stand now.
    fn read(&self, app: &App) -> AccountSnapshot;

    /// Stores a completed authorization as the application's account, dropping the previous
    /// avatar with it.
    fn store_account(&self, app: &mut App, account: AccountConfig);

    /// Forgets the current account and its avatar.
    fn clear_account(&self, app: &mut App);

    /// Stores a generated avatar, unless the account it was generated for is gone; returns
    /// whether it was stored.
    fn store_avatar_if_current(&self, app: &mut App, wallet: &str, svg: String) -> bool;
}

/// What the account service has to do for this control.
///
/// It is a seam, not an abstraction: the control is tested against a fake so that a rejected
/// refresh, an account that switched mid-query and a sign-out that failed can each be produced
/// on demand, and the production implementation is the connector — delegating methods and one
/// conversion (see the [`AccountService`] implementation for [`KytrixServiceConnector`]).
///
/// Every future is boxed because the control drives them on the application's runtime, which is
/// not the runtime the futures' owners were built with.
pub trait AccountService: Send + Sync + 'static {
    /// Generates the key a sign-in is bound to.
    fn prepare_login(&self) -> Result<DpopKey, AuthError>;

    /// Exchanges an authorization code for the account it authorizes.
    fn exchange_authorization_code<'a>(
        &'a self,
        dpop_key: DpopKey,
        code: String,
        redirect_uri: String,
        code_verifier: Zeroizing<String>,
    ) -> BoxFuture<'a, Result<AccountConfig, AuthError>>;

    /// Ends this machine's account session and removes the credential behind it.
    ///
    /// Synchronous because a sign-out is not a request: it happens where the user asked for it,
    /// and it either happened or it did not. What comes back is what
    /// [`Self::revoke_session`] needs to report it, when there is anything to report.
    fn discard_local_session(&self) -> Option<Revocation>;

    /// Reports a finished sign-out to the account service.
    ///
    /// The returned future is what the caller drives on the application runtime, and whose
    /// result nobody reads: the session is already over, and whether the service accepts the
    /// revocation changes nothing about that.
    fn revoke_session<'a>(&'a self, revocation: Revocation) -> BoxFuture<'a, ()>;

    /// The subscription tier the account session holds for `user_id`.
    fn current_plan<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PlanCode>, AuthError>>;

    /// Creates a one-time code that signs this account into the web dashboard.
    fn create_dashboard_handoff(&self) -> BoxFuture<'_, Result<String, AuthError>>;

    /// What the credential stored on this machine says about its own validity.
    fn local_session(&self) -> BoxFuture<'_, Result<LocalSession, AuthError>>;

    /// The session changes every control watches.
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

/// Everything one account control is handed by the composition root.
///
/// See the module documentation for why each handle is a field and for the assembly example.
#[derive(Clone)]
pub struct ProfileHost {
    /// The frozen services: translation, language, settings snapshot, theme, reduced motion,
    /// the open-url command, the tokio runtime and the three composition-root registries
    /// (layer stack, frame router, animation router).
    services: Rc<WidgetServices>,
    /// The account service's OAuth session.
    accounts: Arc<dyn AccountService>,
    /// The store that owns the account record.
    settings: Rc<dyn AccountSettings>,
}

impl ProfileHost {
    /// Assembles a host from the frozen services and the two seams.
    ///
    /// The three kit singletons the menu and its alerts build through are the ones inside
    /// `services` ([`WidgetServices::layer`], [`WidgetServices::frame_router`],
    /// [`WidgetServices::animation_router`]): the control never builds a second layer stack /
    /// frame router / animation router, because the engine has one slot for each and the
    /// composition root already owns it (see `crates/widgets/AGENTS.md` §5/§9).
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

    /// The injected services.
    #[must_use]
    pub fn services(&self) -> &WidgetServices {
        &self.services
    }

    /// The injected services, as the shared handle the host was built from.
    ///
    /// A shell that opens a control from its own context wraps exactly the handle it was handed,
    /// so the control reads the same services the shell renders with.
    #[must_use]
    pub fn services_rc(&self) -> Rc<WidgetServices> {
        Rc::clone(&self.services)
    }

    /// The account service seam.
    #[must_use]
    pub fn accounts(&self) -> &Arc<dyn AccountService> {
        &self.accounts
    }

    /// The account settings seam.
    #[must_use]
    pub fn settings(&self) -> &Rc<dyn AccountSettings> {
        &self.settings
    }

    /// The translation context every interface text is rendered through.
    #[must_use]
    pub fn translate(&self) -> &Translate {
        &self.services.translate
    }

    /// The theme handle; colours are read through it inside `bind_style` closures.
    #[must_use]
    pub fn theme(&self) -> &ThemeHandle {
        &self.services.theme
    }

    /// The settings snapshot signal, which the root keeps in step with its settings manager.
    #[must_use]
    pub fn config_signal(&self) -> ui_core::node::signal::Signal<Config> {
        self.services.config
    }

    /// Runs `read` with a borrowed view of the current settings snapshot.
    pub fn config_ref<R>(&self, app: &App, read: impl FnOnce(&Config) -> R) -> R {
        app.read_with(&self.services.config, read)
    }

    /// The account record and avatar the settings signal currently carries.
    ///
    /// This is the change detector [`ProfileMenu::sync`](crate::profile::ProfileMenu::sync)
    /// compares against; the authoritative snapshot is then read through
    /// [`AccountSettings::read`]. Both read the same record — the root keeps the signal and the
    /// store in step — so the two can only differ for the duration of one write.
    #[must_use]
    pub fn snapshot_signal(&self, app: &App) -> AccountSnapshot {
        self.config_ref(app, |config| AccountSnapshot {
            account: config.account.clone(),
            avatar: config.avatar.as_deref().map(SmolStr::new),
        })
    }

    /// The selected language, as the composition root last wrote it.
    #[must_use]
    pub fn language(&self, app: &App) -> LanguageCode {
        *app.read(&self.services.language)
    }

    /// The language signal `bind_*` closures read to follow a language change.
    #[must_use]
    pub fn language_signal(&self) -> ui_core::node::signal::Signal<LanguageCode> {
        self.services.language
    }

    /// Whether the application asks for reduced motion.
    #[must_use]
    pub fn reduce_motion(&self, app: &App) -> bool {
        *app.read(&self.services.reduce_motion)
    }

    /// Opens an external URL through the application's own command.
    ///
    /// The control never touches a browser or a shell: it asks, and the shell decides.
    pub fn open_url(&self, url: &str) {
        (self.services.open_url)(url);
    }

    /// The tokio runtime the application drives long-running work on.
    #[must_use]
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.services.runtime
    }

    /// The application's layer stack (the one the services carry).
    #[must_use]
    pub fn layer(&self) -> &Layer {
        &self.services.layer
    }

    /// The application's frame router (the one the services carry).
    #[must_use]
    pub fn router(&self) -> &FrameRouter {
        &self.services.frame_router
    }

    /// The application's animation completion router (the one the services carry).
    #[must_use]
    pub fn animation(&self) -> AnimationRouter {
        (*self.services.animation_router).clone()
    }
}
