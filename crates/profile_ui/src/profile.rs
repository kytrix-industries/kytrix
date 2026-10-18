//! The account entry the window header mounts: who is signed in, and how that changes.
//!
//! # What it owns
//!
//! One account, and every flow that can change it: starting a sign-in, waiting for the
//! browser, storing what comes back, asking the account service what tier that account
//! is on, signing out, and noticing when the credential behind the account stops being
//! valid. The avatar is generated here too, because it is a property of the account and
//! has to be dropped with it.
//!
//! # What it does not own
//!
//! The account itself. That is the application settings, and this is a *reader* of them
//! with one writer: [`crate::account`]. Every screen that cares who is signed in —
//! widgets with an account-scoped stream, and this menu — watches the same record, so
//! signing in changes one value and nothing has to be told twice. That is also why
//! nothing here calls `refresh_windows`: settings changes are already published to
//! every watcher.
//!
//! # What it does not do
//!
//! It never retries. A failed sign-in reports itself and stops; the user asks again. A
//! plan that could not be read leaves the menu saying the tier is unknown rather than
//! asking a second time, because the tiers of an account do not change while it is
//! signed in.

use std::sync::Arc;

use app_config::settings::{AccountConfig, AccountWalletKind};
use app_config_provider::manager::ConfigManager;
use base_ui::layer::LayerExt as _;
use futures::future::BoxFuture;
use gpui_kit::*;
use i18n::t;
use kytrix_service_provider::{
    auth::{
        dpop::DpopKey,
        model::{AuthError, PlanCode, SessionChange},
        session::Revocation,
    },
    service::connector::KytrixServiceConnector,
};
use smol_str::SmolStr;
use tokio::{sync::watch, task::AbortHandle};
use widget_tokio_runtime::global;
use zeroize::Zeroizing;

use crate::{
    account::{self, SignedInAccount},
    avatar,
    avatar_dropdown::AvatarDropDown,
    dialogs,
    oauth::{self, LoginError},
    session::{LocalSession, SessionWatch},
};

/// The element id of the header entry, which is also its keyed popup state.
const PROFILE_MENU_ID: &str = "profile-menu";

/// What the account service has to do for this menu.
///
/// It is a seam, not an abstraction: the menu is tested against a fake so that a
/// rejected refresh, an account that switched mid-query and a sign-out that failed can
/// each be produced on demand, and the production implementation is the connector —
/// three delegating methods and one conversion.
pub(crate) trait AccountService: Send + Sync + 'static {
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
    /// Synchronous because a sign-out is not a request: it happens where the user asked
    /// for it, and it either happened or it did not. What comes back is what
    /// [`Self::revoke_session`] needs to report it, when there is anything to report.
    fn discard_local_session(&self) -> Option<Revocation>;

    /// Reports a finished sign-out to the account service.
    ///
    /// The returned future is what the caller drives on a runtime that can make a request
    /// and whose result nobody reads: the session is already over, and whether the service
    /// accepts the revocation changes nothing about that. The executor a view runs on is
    /// not such a runtime — awaiting the report there panics inside the request instead of
    /// sending it.
    fn revoke_session<'a>(&'a self, revocation: Revocation) -> BoxFuture<'a, ()>;

    /// The subscription tier the account session holds for `user_id`.
    fn current_plan<'a>(
        &'a self,
        user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PlanCode>, AuthError>>;

    /// What the credential stored on this machine says about its own validity.
    fn local_session(&self) -> BoxFuture<'_, Result<LocalSession, AuthError>>;

    /// The session changes every open menu watches.
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

    fn session_changes(&self) -> watch::Receiver<SessionChange> {
        self.subscribe_account_session_changes()
    }
}

/// The account entry in the window header.
pub struct ProfileMenu {
    source: Arc<dyn AccountService>,
    /// The account in the settings, which is the one this menu is about.
    account: Option<AccountConfig>,
    /// The avatar of that account, decoded once per settings change.
    avatar: Option<Arc<Image>>,
    /// The wallet an avatar is being generated for, so a second request for the same
    /// face is not started and a result for a replaced account is not drawn.
    avatar_wallet: Option<SmolStr>,
    plan: Option<PlanCode>,
    /// Bumped whenever the plan a query was for stops being the plan in question.
    plan_generation: u64,
    /// The task of the attempt that is waiting for its browser callback, if one is.
    ///
    /// Ending the attempt is aborting this: the listener the attempt owns is released where
    /// it is dropped, and the task this menu spawned to wait on it answers with a
    /// cancellation.
    sign_in: Option<AbortHandle>,
    _account_task: Task<()>,
    _session_task: Task<()>,
    avatar_task: Option<Task<()>>,
    plan_task: Option<Task<()>>,
    sign_in_task: Option<Task<()>>,
}

impl ProfileMenu {
    /// Builds the menu the application mounts, reading the account service from the
    /// globals the composition root installed.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let source = Arc::new(cx.global::<KytrixServiceConnector>().clone());

        Self::with_source(source, cx)
    }

    fn with_source(source: Arc<dyn AccountService>, cx: &mut Context<Self>) -> Self {
        let mut changes = cx.global::<ConfigManager>().subscribe();
        let account_task = cx.spawn(async move |menu, cx| {
            loop {
                if changes.changed().await.is_err() {
                    return;
                }
                let Ok(()) = menu.update(cx, |menu, cx| {
                    menu.refresh_account(cx);
                    cx.notify();
                }) else {
                    return;
                };
            }
        });

        let session_source = Arc::clone(&source);
        let session_task = cx.spawn(async move |menu, cx| {
            let queried = session_source.local_session().await;
            let mut watch = SessionWatch::new(session_source.session_changes());
            let mut clears_account = watch.reconcile(queried);
            loop {
                if clears_account {
                    let Ok(()) = menu.update(cx, ProfileMenu::forget_invalidated_account) else {
                        return;
                    };
                }
                let Some(next) = watch.changed().await else {
                    return;
                };
                clears_account = next;
            }
        });

        let mut menu = Self {
            source,
            account: None,
            avatar: None,
            avatar_wallet: None,
            plan: None,
            plan_generation: 0,
            sign_in: None,
            _account_task: account_task,
            _session_task: session_task,
            avatar_task: None,
            plan_task: None,
            sign_in_task: None,
        };
        menu.refresh_account(cx);
        menu
    }

    /// Re-reads the account from the settings and follows what changed about it.
    fn refresh_account(&mut self, cx: &mut Context<Self>) {
        let previous_user_id = self.account.as_ref().map(|account| account.user_id.clone());
        let (account, avatar) = {
            let config = cx.global::<ConfigManager>().read();
            let avatar = account::avatar_of(&config)
                .map(|svg| Arc::new(Image::from_bytes(ImageFormat::Svg, svg.as_bytes().to_vec())));

            (config.account.clone(), avatar)
        };
        let current_user_id = account.as_ref().map(|account| account.user_id.clone());
        self.account = account;
        self.avatar = avatar;

        if previous_user_id != current_user_id {
            self.clear_plan_query();
            if let Some(user_id) = current_user_id {
                self.start_plan_query(user_id, cx);
            }
        }
        if let Some(account) = self.account.as_ref()
            && self.avatar.is_none()
        {
            self.generate_avatar(SmolStr::new(account.wallet_address.as_str()), cx);
        }
    }

    /// Forgets an account whose credential the account session has invalidated.
    fn forget_invalidated_account(&mut self, cx: &mut Context<Self>) {
        if self.account.is_none() {
            return;
        }

        account::clear_account(cx);
    }

    /// Reads the subscription tier of an account.
    ///
    /// The tier is reported against the generation it was asked for: a query that
    /// outlives the account it was about would otherwise label the next account.
    fn start_plan_query(&mut self, user_id: String, cx: &mut Context<Self>) {
        let generation = self.plan_generation;
        let source = Arc::clone(&self.source);
        self.plan_task.take();
        self.plan_task = Some(cx.spawn(async move |menu, cx| {
            let result = source.current_plan(&user_id).await;
            let _ = menu.update(cx, |menu, cx| {
                let is_current = menu.plan_generation == generation
                    && menu
                        .account
                        .as_ref()
                        .is_some_and(|account| account.user_id == user_id);
                if !is_current {
                    return;
                }
                menu.plan_task = None;
                match result {
                    Ok(plan) => menu.plan = plan,
                    Err(error) => {
                        menu.plan = None;
                        log::warn!("failed to inspect the active account plan: {error}");
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Stops believing the tier that is currently on screen.
    fn clear_plan_query(&mut self) {
        self.plan_generation = self.plan_generation.wrapping_add(1);
        self.plan_task.take();
        self.plan = None;
    }

    /// Generates the avatar of a wallet in the background.
    fn generate_avatar(&mut self, wallet: SmolStr, cx: &mut Context<Self>) {
        if self.avatar_wallet.as_ref() == Some(&wallet) {
            return;
        }
        self.avatar_wallet = Some(wallet.clone());
        let task =
            global::handle(cx).run(avatar::generate_avatar(wallet.clone(), global::handle(cx)));
        self.avatar_task.take();
        self.avatar_task = Some(cx.spawn(async move |menu, cx| {
            let Ok(result) = task.await else {
                log::error!("the wallet avatar task failed");
                return;
            };
            let _ = menu.update(cx, |menu, cx| menu.apply_avatar(&wallet, result, cx));
        }));
    }

    /// Accepts a generated avatar, unless the account it was for is no longer the one
    /// signed in.
    fn apply_avatar(
        &mut self,
        wallet: &SmolStr,
        result: Result<(SmolStr, String), avatar::AvatarError>,
        cx: &mut Context<Self>,
    ) {
        if self.avatar_wallet.as_ref() != Some(wallet) {
            return;
        }
        self.avatar_wallet = None;
        self.avatar_task.take();

        match result {
            // The settings publish the new avatar, and this menu reads it back with
            // every other watcher, so the face and the record cannot disagree.
            Ok((wallet, svg)) => {
                if !account::store_avatar_if_current(&wallet, svg, cx) {
                    log::warn!("a generated avatar no longer belonged to the signed-in account");
                }
            }
            Err(error) => log::warn!("failed to generate the wallet avatar: {error}"),
        }
    }

    /// Starts a sign-in: prepares the attempt, opens the browser, and waits.
    ///
    /// The attempt lives exactly as long as the alert that reports it. Reading that alert is
    /// how the user says they are done with this attempt, so the alert aborts it — there is
    /// nothing behind it any more, and the menu offers a sign-in again the moment the alert
    /// is gone. An attempt nothing is waiting on would be a listener holding a loopback port
    /// for five minutes while the user has no way to ask for another one.
    ///
    /// A sign-in that cannot even be prepared says so in the same alert the wait would
    /// have used: the user asked for something and is owed the reason it did not
    /// happen, in the place they asked from.
    fn start_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Whoever asks for a sign-in asks for the one they are about to get: an attempt left
        // over from the last time is ended here rather than left to be reported later, over
        // the alert this one is about to open.
        self.abort_pending_sign_in();

        let dpop_key = match self.source.prepare_login() {
            Ok(dpop_key) => dpop_key,
            Err(error) => {
                let reason = oauth::service_failure(&error, cx);
                dialogs::show_authorization_failed(window, cx, reason);
                return;
            }
        };
        let attempt = match oauth::prepare_login(dpop_key) {
            Ok(attempt) => attempt,
            Err(error) => {
                let reason = error.message(cx);
                dialogs::show_authorization_failed(window, cx, reason);
                return;
            }
        };
        let authorization_url = attempt.authorization_url().to_owned();
        let source = Arc::clone(&self.source);
        let login = global::handle(cx).run(oauth::run_login(
            attempt,
            move |dpop_key, code, redirect_uri, code_verifier| {
                let source = Arc::clone(&source);
                async move {
                    source
                        .exchange_authorization_code(dpop_key, code, redirect_uri, code_verifier)
                        .await
                }
            },
        ));

        // The alert is the attempt's own report, so what it aborts is taken here, before the
        // alert exists to be read. It is a handle to the same task, which this menu ends the
        // attempt with wherever else an ending is asked for.
        let abort = login.abort_handle();
        let reported = abort.clone();
        self.sign_in = Some(abort);

        let window_handle = window.window_handle();
        self.sign_in_task.take();
        self.sign_in_task = Some(cx.spawn(async move |menu, cx| {
            let outcome = match login.await {
                Ok(result) => Some(result),
                Err(error) if error.is_cancelled() => None,
                Err(error) => {
                    log::error!("the sign-in task failed: {error}");
                    None
                }
            };
            let _ = cx.update_window(window_handle, move |_, window, cx| {
                let window = window.window_handle();
                let _ = menu.update(cx, |menu, cx| menu.finish_sign_in(outcome, window, cx));
            });
        }));

        dialogs::show_awaiting_authorization(window, cx, reported);
        cx.open_url(&authorization_url);
        cx.notify();
    }

    /// Accepts the end of the attempt that was waiting for its browser callback.
    ///
    /// The wait is over whichever way it ended, so the attempt is dropped first: the menu
    /// offers a sign-in again from here, whether the account arrived, the attempt failed,
    /// or nothing was left to report. The account is stored in the same step, because it is
    /// the news itself rather than something drawn about it.
    ///
    /// An attempt that ends with nothing to report — the user read its alert, or it expired
    /// — still ends with whatever alert of its is left taken down, because the wait is over
    /// and there is nothing left to wait for it in. An alert the user read is already gone,
    /// and taking that one down again is a dismissal of nothing.
    ///
    /// The dialog is deferred rather than opened here, because this can be reached while
    /// the window is in the middle of an update — a callback that arrives as the user
    /// signs out, a task resumed by the turn that started the attempt — and GPUI answers a
    /// window update from inside one with "window not found". Deferring it puts the report
    /// in the effect cycle that follows, where the window is the window again; the account
    /// is stored here rather than there so that nothing about it waits on a window.
    fn finish_sign_in(
        &mut self,
        outcome: Option<Result<AccountConfig, LoginError>>,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.sign_in = None;

        // The account is stored here and what is drawn about it is deferred: the record is
        // the news itself, and nothing about keeping it should wait on a window.
        let mut authorized = None;
        let revoked = match outcome {
            Some(Ok(account)) => {
                authorized = Some((
                    SharedString::from(account.wallet_address.as_str()),
                    SharedString::new_static(wallet_kind_label(account.wallet_kind)),
                ));
                account::store_account(account, cx);

                None
            }
            // The reason is read here rather than where it is shown: it is copy produced
            // where the failure happened, so a language that changes in between cannot
            // change what this attempt is reported in.
            Some(Err(error)) => Some(error.message(cx)),
            None => None,
        };

        cx.defer(move |cx| {
            let _ = cx.update_window(window, |_, window, cx| {
                if let Some((wallet_address, wallet_kind)) = authorized {
                    dialogs::show_authorization_complete(window, cx, wallet_address, wallet_kind);
                } else if let Some(reason) = revoked {
                    // The alert the user is watching becomes the report: replacing it in
                    // place says what went wrong without asking them to go looking.
                    dialogs::show_authorization_failed(window, cx, reason);
                } else {
                    // Nothing to report: the attempt expired, or the user read the alert
                    // already. An alert of this attempt's is taken down either way, which is
                    // a dismissal of nothing when it is the one that was read.
                    window.dismiss_alert(cx);
                }
            });
        });
    }

    /// Ends an attempt whose browser callback has not arrived.
    ///
    /// Everything the attempt holds — the loopback listener above all — is released by
    /// aborting it, and the task waiting on it answers with the cancellation this menu
    /// reports as the end of the wait.
    fn abort_pending_sign_in(&mut self) {
        if let Some(sign_in) = self.sign_in.take() {
            sign_in.abort();
        }
    }
    /// Asks whether the account should be signed out.
    fn confirm_sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.account.is_none() {
            return;
        }
        let menu = cx.entity().downgrade();
        dialogs::show_sign_out_confirmation(window, cx, move |_, cx| {
            let _ = menu.update(cx, |menu, cx| menu.sign_out(cx));
        });
    }

    /// Ends the account session and forgets the account, here and now.
    ///
    /// Nothing is asked of the account service first and nothing is waited for: the
    /// credential this machine holds is removed, the session behind it is retired, and
    /// the account is dropped from the settings, all before this returns. Telling the
    /// service is a report that follows, on a task nobody waits for — which is what makes
    /// a sign-out impossible to fail.
    ///
    /// A sign-in still waiting for its browser is ended with the session: a callback that
    /// arrived after this would sign the user back into the account they just left.
    fn sign_out(&mut self, cx: &mut Context<Self>) {
        if self.account.is_none() {
            return;
        }
        self.clear_plan_query();
        self.abort_pending_sign_in();

        let revocation = self.source.discard_local_session();
        account::clear_account(cx);

        if let Some(revocation) = revocation {
            let source = Arc::clone(&self.source);
            // Telling the account service is a request, and the executor this menu runs on
            // has no reactor to drive one: it is handed to the application runtime, like
            // every other call this menu makes to the service.
            global::handle(cx).run(async move { source.revoke_session(revocation).await });
        }
        cx.notify();
    }
}

impl Drop for ProfileMenu {
    fn drop(&mut self) {
        // A window that goes away abandons its attempt: nothing is left listening for a
        // callback for a menu that no longer exists. The task waiting on it is dropped with
        // the menu, so nothing is left to report the end of that wait either.
        self.sign_in_task.take();
        self.abort_pending_sign_in();
    }
}

impl Render for ProfileMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = cx.entity().downgrade();
        let sign_in = menu.clone();
        let sign_out = menu;

        AvatarDropDown::new(PROFILE_MENU_ID, t!(cx, "window.account.menu"))
            .account(self.account.as_ref().map(SignedInAccount::from))
            .avatar(self.avatar.clone())
            .plan(self.plan)
            .on_sign_in(move |window, cx| {
                let _ = sign_in.update(cx, |menu, cx| menu.start_sign_in(window, cx));
            })
            .on_sign_out(move |window, cx| {
                let _ = sign_out.update(cx, |menu, cx| menu.confirm_sign_out(window, cx));
            })
    }
}

/// The wallet family of a configured account, as copy.
const fn wallet_kind_label(wallet_kind: AccountWalletKind) -> &'static str {
    match wallet_kind {
        AccountWalletKind::Ethereum => "Ethereum",
        AccountWalletKind::Solana => "Solana",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use app_config_provider::manager::ConfigManager;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer};
    use futures::future::BoxFuture;
    use gpui_kit::{
        AppContext as _, BorrowAppContext as _, Context, Entity, IntoElement, Modifiers,
        ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window, div,
        px, size,
    };
    use kytrix_service_provider::auth::{
        dpop::DpopKey,
        model::{AuthError, PlanCode, SessionAvailability, SessionChange},
        session::Revocation,
    };
    use smol_str::SmolStr;
    use tokio::sync::watch;
    use widget_tokio_runtime::global;

    use super::{AccountService, LoginError, ProfileMenu};
    use crate::{
        account::shortened_wallet_address,
        avatar_dropdown::{ProfileAction, profile_action},
        session::LocalSession,
    };

    const ETHEREUM: &str = "0x0123456789abcdef0123456789abcdef01234567";
    const SOLANA: &str = "7YttLkHDoNj9wyDur5VjZ9Pd7WZZvnGvWKbxzwfKQnDb";

    const FAILURE: AuthError = AuthError::MissingCredential;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Step {
        Succeed,
        Fail,
    }

    struct FakeState {
        prepare: Step,
        exchange: Step,
        plan: Option<Option<PlanCode>>,
        plan_step: Step,
        /// What the credential on disk says; `None` is a query that failed.
        local: Option<SessionAvailability>,
        /// What the session hands over when it is signed out.
        revocation: Option<Revocation>,
        /// Where the next sign-out report says which runtime drove it. A test that waits on
        /// it knows the report was made, which the counters alone cannot say: a counter
        /// moves when a future is asked for, not when it is driven.
        revoke_report: Option<std::sync::mpsc::Sender<bool>>,
        prepared: usize,
        exchanges: usize,
        discards: usize,
        revocations: usize,
    }

    impl FakeState {
        fn new() -> Self {
            Self {
                prepare: Step::Succeed,
                exchange: Step::Succeed,
                plan: Some(Some(PlanCode::Plus)),
                plan_step: Step::Succeed,
                local: Some(SessionAvailability::Available),
                revocation: Some(Revocation::new(
                    "refresh-token",
                    DpopKey::generate().expect("a test DPoP key should generate"),
                )),
                revoke_report: None,
                prepared: 0,
                exchanges: 0,
                discards: 0,
                revocations: 0,
            }
        }
    }

    #[derive(Clone)]
    struct FakeAccountService {
        state: Arc<Mutex<FakeState>>,
        changes: watch::Sender<SessionChange>,
        exchanged: AccountConfig,
    }

    impl FakeAccountService {
        fn new(exchanged: AccountConfig) -> Self {
            let (changes, _) = watch::channel(SessionChange::default());

            Self {
                state: Arc::new(Mutex::new(FakeState::new())),
                changes,
                exchanged,
            }
        }

        fn with<R>(&self, update: impl FnOnce(&mut FakeState) -> R) -> R {
            update(
                &mut self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }

        fn count(&self, count: impl FnOnce(&FakeState) -> usize) -> usize {
            self.with(|state| count(state))
        }
    }

    impl AccountService for FakeAccountService {
        fn prepare_login(&self) -> Result<DpopKey, AuthError> {
            match self.with(|state| {
                state.prepared += 1;

                state.prepare
            }) {
                Step::Succeed => DpopKey::generate(),
                Step::Fail => Err(FAILURE),
            }
        }

        fn exchange_authorization_code<'a>(
            &'a self,
            _dpop_key: DpopKey,
            _code: String,
            _redirect_uri: String,
            _code_verifier: zeroize::Zeroizing<String>,
        ) -> BoxFuture<'a, Result<AccountConfig, AuthError>> {
            let result = self.with(|state| {
                state.exchanges += 1;

                match state.exchange {
                    Step::Succeed => Ok(self.exchanged.clone()),
                    Step::Fail => Err(FAILURE),
                }
            });

            Box::pin(async move { result })
        }

        fn discard_local_session(&self) -> Option<Revocation> {
            self.with(|state| {
                state.discards += 1;

                state.revocation.clone()
            })
        }

        fn revoke_session<'a>(&'a self, _revocation: Revocation) -> BoxFuture<'a, ()> {
            let report = self.with(|state| {
                state.revocations += 1;

                state.revoke_report.take()
            });

            Box::pin(async move {
                if let Some(report) = report {
                    // The production report is a request to the account service, and the
                    // executor a view runs on is not a runtime that can make one: asking
                    // here is what tells the difference apart.
                    let _ = report.send(tokio::runtime::Handle::try_current().is_ok());
                }
            })
        }

        fn current_plan<'a>(
            &'a self,
            _user_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<PlanCode>, AuthError>> {
            let result = self.with(|state| match state.plan_step {
                Step::Succeed => Ok(state.plan.flatten()),
                Step::Fail => Err(FAILURE),
            });

            Box::pin(async move { result })
        }

        fn local_session(&self) -> BoxFuture<'_, Result<LocalSession, AuthError>> {
            let result = self.with(|state| match state.local {
                Some(availability) => Ok(LocalSession::new(availability, SessionChange::default())),
                None => Err(FAILURE),
            });

            Box::pin(async move { result })
        }

        fn session_changes(&self) -> watch::Receiver<SessionChange> {
            self.changes.subscribe()
        }
    }

    fn account(wallet_address: &str, wallet_kind: AccountWalletKind) -> AccountConfig {
        AccountConfig {
            user_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            wallet_address: wallet_address.to_owned(),
            wallet_kind,
        }
    }

    /// A second account, which is what makes an account *change* observable.
    fn other_account() -> AccountConfig {
        let mut account = account(SOLANA, AccountWalletKind::Solana);
        account.user_id = "22222222-2222-4222-8222-222222222222".to_owned();
        account
    }

    fn ethereum_account() -> AccountConfig {
        account(ETHEREUM, AccountWalletKind::Ethereum)
    }

    /// A settings snapshot holding `account`, with no avatar generated yet.
    fn configured(account: AccountConfig) -> Config {
        let mut config = Config::default();
        config.account = Some(account);
        config
    }

    /// The menu as the header mounts it, over an in-memory settings snapshot.
    struct Harness<'a> {
        menu: Entity<ProfileMenu>,
        source: FakeAccountService,
        cx: &'a mut VisualTestContext,
    }

    struct MenuRoot {
        menu: Entity<ProfileMenu>,
    }

    impl Render for MenuRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // The menu is mounted the way the header mounts it: a flex row that sizes the
            // trigger to its own button and puts it at the end of the row. A trigger left
            // to stretch across the window would take a press anywhere along it.
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_end()
                .child(self.menu.clone())
        }
    }

    fn harness<'a>(
        cx: &'a mut TestAppContext,
        config: Config,
        source: FakeAccountService,
    ) -> Harness<'a> {
        cx.executor().allow_parking();
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("test Tokio runtime should build");
            cx.set_global(ConfigManager::in_memory(config));
        });
        let menu =
            cx.update(|cx| cx.new(|cx| ProfileMenu::with_source(Arc::new(source.clone()), cx)));
        let rendered = menu.clone();
        let (_, cx) = cx.add_window_view(move |_, cx| {
            let root = cx.new(|_| MenuRoot { menu: rendered });

            Layer::new(root)
        });
        cx.simulate_resize(size(px(800.0), px(600.0)));
        cx.run_until_parked();

        Harness { menu, source, cx }
    }

    fn signed_in_harness(cx: &mut TestAppContext) -> Harness<'_> {
        harness(
            cx,
            configured(ethereum_account()),
            FakeAccountService::new(ethereum_account()),
        )
    }

    /// Opens the menu by clicking the avatar and repaints.
    fn open_menu(harness: &mut Harness) {
        let trigger = harness
            .cx
            .debug_bounds("TEST_PROFILE_TRIGGER")
            .expect("the trigger should render");
        harness
            .cx
            .simulate_click(trigger.center(), Modifiers::none());
        harness.cx.run_until_parked();
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();
    }

    fn shows(harness: &mut Harness, selector: &'static str) -> bool {
        harness.cx.debug_bounds(selector).is_some()
    }

    /// Lets a fade finish and repaints, so what is drawn is what has settled: a popup in
    /// its first frames is painted at zero opacity and cannot be told from one that stays.
    fn settle(harness: &mut Harness) {
        harness
            .cx
            .executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();
    }

    fn stored_account(harness: &mut Harness) -> Option<AccountConfig> {
        harness
            .cx
            .update(|_, cx| cx.global::<ConfigManager>().read().account.clone())
    }

    /// Watches the report the next sign-out makes to the account service.
    fn watch_sign_out_report(source: &FakeAccountService) -> std::sync::mpsc::Receiver<bool> {
        let (report, observed) = std::sync::mpsc::channel();
        source.with(|state| state.revoke_report = Some(report));

        observed
    }

    /// Waits for a watched sign-out report, and says whether a runtime drove it.
    ///
    /// The deadline is what keeps a report that is never driven from looking like one that
    /// is still on its way.
    fn wait_for_sign_out_report(observed: std::sync::mpsc::Receiver<bool>) -> bool {
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("the sign-out report should be made before the deadline")
    }

    #[test]
    fn every_menu_entry_should_map_to_the_action_it_stands_for() {
        assert_eq!(profile_action("sign-in"), Some(ProfileAction::SignIn));
        assert_eq!(profile_action("sign-out"), Some(ProfileAction::SignOut));
        assert_eq!(profile_action("something-else"), None);
    }

    #[gpui_kit::test]
    fn a_menu_with_nobody_signed_in_should_offer_only_a_sign_in(cx: &mut TestAppContext) {
        let mut harness = harness(
            cx,
            Config::default(),
            FakeAccountService::new(ethereum_account()),
        );

        assert!(!shows(&mut harness, "TEST_PROFILE_ACCOUNT"));
        open_menu(&mut harness);

        assert!(shows(&mut harness, "TEST_PROFILE_MENU"));
        assert!(!shows(&mut harness, "TEST_PROFILE_ACCOUNT"));
    }

    #[gpui_kit::test]
    fn a_signed_in_menu_should_show_the_account_and_its_tier(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();
        open_menu(&mut harness);

        assert!(shows(&mut harness, "TEST_PROFILE_ACCOUNT"));
        assert!(shows(&mut harness, "TEST_PROFILE_ADDRESS"));
        assert!(shows(&mut harness, "TEST_PROFILE_BRAND"));
        assert!(shows(&mut harness, "TEST_PROFILE_PLAN_PLUS"));
    }

    /// The account block is a column: the face with the wallet family's mark on its
    /// lower-right corner, the tier, and the address, each line centred on the menu, with
    /// the button that copies the address on the right of it.
    #[gpui_kit::test]
    fn a_signed_in_menu_should_stack_the_account_block_top_to_bottom(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();
        open_menu(&mut harness);

        let drawn = |harness: &mut Harness, selector: &'static str| {
            harness
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("the account block should draw {selector}"))
        };

        let panel = drawn(&mut harness, "TEST_PROFILE_MENU");
        let avatar = drawn(&mut harness, "TEST_PROFILE_ACCOUNT_AVATAR");
        let badge = drawn(&mut harness, "TEST_PROFILE_BRAND");
        let tier = drawn(&mut harness, "TEST_PROFILE_PLAN_PLUS");
        let address = drawn(&mut harness, "TEST_PROFILE_ADDRESS");
        let copy = drawn(&mut harness, "TEST_PROFILE_COPY");

        assert_eq!(
            avatar.size,
            gpui_kit::size(gpui_kit::px(56.0), gpui_kit::px(56.0)),
            "the face of the account block is the largest thing in it"
        );
        assert!(
            avatar.bottom() <= tier.top(),
            "the tier belongs under the face"
        );
        assert!(
            tier.bottom() <= address.top(),
            "the address belongs under the tier"
        );

        // The wallet family is a badge on the face: it belongs to the lower-right corner of
        // the avatar rather than to a line beside the address, and it reaches past the
        // face's own edge, which is what makes it read as a badge on the corner.
        assert_eq!(
            badge.size,
            gpui_kit::size(gpui_kit::px(20.0), gpui_kit::px(20.0)),
            "the wallet family is a corner mark, not a line of its own"
        );
        assert!(
            badge.center().x > avatar.center().x && badge.center().y > avatar.center().y,
            "the badge belongs on the lower-right corner of the face"
        );
        assert!(
            badge.left() < avatar.right() && badge.top() < avatar.bottom(),
            "the badge sits on the face rather than beside it"
        );
        assert!(
            badge.right() > avatar.right() && badge.bottom() > avatar.bottom(),
            "the badge reaches past the edge of the face"
        );
        assert!(
            badge.right() <= panel.right() && badge.bottom() <= panel.bottom(),
            "a badge on the face may not hang out of the menu"
        );

        // Both sides of the address are given the same room rather than the address being
        // padded by whatever sits beside it, so the address holds the centre of the menu.
        // Splitting that room rounds to whole pixels, which is why this is not exact.
        for line in [avatar, tier, address] {
            assert!(
                (line.center().x - panel.center().x).abs() <= gpui_kit::px(2.0),
                "every line is centred on the menu"
            );
        }
        assert!(
            copy.left() >= address.right(),
            "the copy button sits to the right of the address"
        );
        assert!(
            copy.right() <= panel.right(),
            "the copy button may not leave the menu"
        );
    }

    /// The badge belongs to the face the menu draws and not to the trigger: the header
    /// holds the same avatar, and a mark on it there would be a claim about a menu nobody
    /// has opened.
    #[gpui_kit::test]
    fn the_wallet_badge_should_belong_to_the_menu_rather_than_to_the_trigger(
        cx: &mut TestAppContext,
    ) {
        let mut harness = signed_in_harness(cx);
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();

        assert!(shows(&mut harness, "TEST_PROFILE_TRIGGER_AVATAR"));
        assert!(
            !shows(&mut harness, "TEST_PROFILE_BRAND"),
            "the trigger of a closed menu carries no badge"
        );

        open_menu(&mut harness);

        let trigger = harness
            .cx
            .debug_bounds("TEST_PROFILE_TRIGGER_AVATAR")
            .expect("the trigger keeps the avatar it had");
        let badge = harness
            .cx
            .debug_bounds("TEST_PROFILE_BRAND")
            .expect("the face in the menu carries the wallet family's mark");

        assert!(
            badge.top() > trigger.bottom(),
            "the badge is drawn on the face in the menu, not on the trigger"
        );
    }

    /// The address on the line is the shortened one, so hovering it is how the user reads
    /// the whole thing without copying it.
    #[gpui_kit::test]
    fn hovering_the_address_should_offer_the_complete_one(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();
        open_menu(&mut harness);

        let address = harness
            .cx
            .debug_bounds("TEST_PROFILE_ADDRESS")
            .expect("the account block should draw the shortened address");
        settle(&mut harness);
        assert!(
            !shows(&mut harness, "TEST_PROFILE_ADDRESS_TOOLTIP"),
            "a tooltip nobody hovered is a tooltip in the way of what is behind it"
        );

        harness
            .cx
            .simulate_mouse_move(address.center(), None, Modifiers::none());
        settle(&mut harness);

        let tooltip = harness
            .cx
            .debug_bounds("TEST_PROFILE_ADDRESS_TOOLTIP")
            .expect("hovering the address should show the complete one");
        assert!(
            tooltip.size.width > address.size.width,
            "the tooltip carries more of the address than the line has room for"
        );
    }

    #[gpui_kit::test]
    fn an_unknown_tier_should_be_shown_as_a_stand_in_rather_than_left_blank(
        cx: &mut TestAppContext,
    ) {
        let source = FakeAccountService::new(ethereum_account());
        source.with(|state| state.plan = Some(None));
        let mut harness = harness(cx, configured(ethereum_account()), source);
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();
        open_menu(&mut harness);

        assert!(shows(&mut harness, "TEST_PROFILE_PLAN_UNKNOWN"));
        assert!(!shows(&mut harness, "TEST_PROFILE_PLAN_PLUS"));
    }

    #[gpui_kit::test]
    fn the_copy_entry_should_copy_the_complete_address(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();
        open_menu(&mut harness);

        let copy = harness
            .cx
            .debug_bounds("TEST_PROFILE_COPY")
            .expect("the account row should offer a copy entry");
        harness.cx.simulate_click(copy.center(), Modifiers::none());

        assert_eq!(
            harness
                .cx
                .read_from_clipboard()
                .and_then(|item| item.text()),
            Some(ETHEREUM.to_owned())
        );
        assert_ne!(
            shortened_wallet_address(ETHEREUM, AccountWalletKind::Ethereum),
            ETHEREUM
        );
    }

    #[gpui_kit::test]
    fn a_sign_in_that_cannot_be_prepared_should_say_so_in_an_alert(cx: &mut TestAppContext) {
        let mut harness = harness(
            cx,
            Config::default(),
            FakeAccountService::new(ethereum_account()),
        );
        harness.source.with(|state| state.prepare = Step::Fail);

        harness.cx.update(|window, cx| {
            harness
                .menu
                .update(cx, |menu, cx| menu.start_sign_in(window, cx));
        });
        harness.cx.run_until_parked();

        assert!(shows(&mut harness, "BASE_UI_ALERT"));
        assert!(
            !pending_sign_in(&mut harness),
            "a sign-in that was never opened has nothing to wait for"
        );
        assert!(stored_account(&mut harness).is_none());
    }

    #[gpui_kit::test]
    fn starting_a_sign_in_should_open_the_browser_and_wait_for_it(cx: &mut TestAppContext) {
        let mut harness = harness(
            cx,
            Config::default(),
            FakeAccountService::new(ethereum_account()),
        );

        start_sign_in(&mut harness);

        assert_eq!(harness.source.count(|state| state.prepared), 1);
        assert!(shows(&mut harness, "BASE_UI_ALERT"));
        assert!(
            pending_sign_in(&mut harness),
            "the alert reports a wait, it does not end one"
        );
    }

    /// Reading the waiting alert is the user's answer to the whole attempt: the alert is
    /// what reports the wait, so the wait ends where the report was read. The attempt is
    /// aborted rather than left listening, which is what lets the menu offer a sign-in
    /// again immediately instead of greying the entry out until the attempt expires.
    #[gpui_kit::test]
    fn reading_the_waiting_alert_should_end_the_attempt(cx: &mut TestAppContext) {
        let mut harness = harness(
            cx,
            Config::default(),
            FakeAccountService::new(ethereum_account()),
        );
        start_sign_in(&mut harness);
        let ok = harness
            .cx
            .debug_bounds("BASE_UI_ALERT_OK")
            .expect("the waiting alert should offer its one button");

        harness.cx.simulate_click(ok.center(), Modifiers::none());
        settle(&mut harness);

        assert!(!shows(&mut harness, "BASE_UI_ALERT"));
        assert!(
            !pending_sign_in(&mut harness),
            "reading the alert is how the user ends the attempt behind it"
        );
        assert!(stored_account(&mut harness).is_none());
    }

    /// The reported bug: a sign-in whose alert was read must not leave the menu unable to
    /// start another one. The attempt is aborted where the alert is read, so the second
    /// sign-in is a fresh attempt rather than a report over an attempt still running.
    #[gpui_kit::test]
    fn a_sign_in_should_be_offered_again_once_its_alert_was_read(cx: &mut TestAppContext) {
        let mut harness = harness(
            cx,
            Config::default(),
            FakeAccountService::new(ethereum_account()),
        );
        start_sign_in(&mut harness);
        let ok = harness
            .cx
            .debug_bounds("BASE_UI_ALERT_OK")
            .expect("the waiting alert should offer its one button");

        harness.cx.simulate_click(ok.center(), Modifiers::none());
        settle(&mut harness);

        start_sign_in(&mut harness);

        assert_eq!(
            harness.source.count(|state| state.prepared),
            2,
            "the second sign-in has to be a new attempt, not the first one still running"
        );
        assert!(pending_sign_in(&mut harness));
        assert!(
            shows(&mut harness, "BASE_UI_ALERT"),
            "the attempt the second sign-in started reports itself in its own alert"
        );
    }

    /// Signing out ends a sign-in that is still waiting: a callback that arrived afterwards
    /// would sign the user back into the account they just left. The alert goes with the
    /// attempt it reported, because the user did not answer it.
    #[gpui_kit::test]
    fn signing_out_should_end_a_sign_in_that_is_still_waiting(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);
        start_sign_in(&mut harness);

        harness.cx.update(|_, cx| {
            harness.menu.update(cx, |menu, cx| menu.sign_out(cx));
        });
        settle(&mut harness);

        assert!(!pending_sign_in(&mut harness));
        assert!(
            !shows(&mut harness, "BASE_UI_ALERT"),
            "a sign-out the user asked for is not a failed sign-in to report"
        );
        assert!(stored_account(&mut harness).is_none());
    }

    #[gpui_kit::test]
    fn an_authorized_sign_in_should_store_the_account_and_report_it(cx: &mut TestAppContext) {
        let mut harness = harness(
            cx,
            Config::default(),
            FakeAccountService::new(ethereum_account()),
        );
        start_sign_in(&mut harness);

        finish_sign_in(&mut harness, Some(Ok(ethereum_account())));

        assert_eq!(
            stored_account(&mut harness).as_ref(),
            Some(&ethereum_account())
        );
        assert!(shows(&mut harness, "BASE_UI_ALERT"));
        assert!(
            !pending_sign_in(&mut harness),
            "the alert that reported the account is the end of the wait"
        );
    }

    #[gpui_kit::test]
    fn a_rejected_sign_in_should_be_reported_where_the_user_is_watching(cx: &mut TestAppContext) {
        let mut harness = harness(
            cx,
            Config::default(),
            FakeAccountService::new(ethereum_account()),
        );
        start_sign_in(&mut harness);

        finish_sign_in(&mut harness, Some(Err(LoginError::Expired)));

        assert!(stored_account(&mut harness).is_none());
        assert!(shows(&mut harness, "BASE_UI_ALERT"));
        assert!(!pending_sign_in(&mut harness));
    }

    #[gpui_kit::test]
    fn a_failure_report_should_be_closed_by_its_button(cx: &mut TestAppContext) {
        let mut harness = harness(
            cx,
            Config::default(),
            FakeAccountService::new(ethereum_account()),
        );
        start_sign_in(&mut harness);
        finish_sign_in(&mut harness, Some(Err(LoginError::Expired)));

        let ok = harness
            .cx
            .debug_bounds("BASE_UI_ALERT_OK")
            .expect("the failure report should offer its one button");
        harness.cx.simulate_click(ok.center(), Modifiers::none());
        settle(&mut harness);

        assert!(!shows(&mut harness, "BASE_UI_ALERT"));
    }

    #[gpui_kit::test]
    fn signing_out_should_ask_first_and_only_then_forget_the_account(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);
        harness.cx.update(|window, cx| {
            harness
                .menu
                .update(cx, |menu, cx| menu.confirm_sign_out(window, cx));
        });
        harness.cx.run_until_parked();

        assert!(shows(&mut harness, "BASE_UI_ALERT"));
        assert_eq!(harness.source.count(|state| state.discards), 0);

        let cancel = harness
            .cx
            .debug_bounds("BASE_UI_ALERT_CANCEL")
            .expect("the confirmation should offer a cancel entry");
        harness
            .cx
            .simulate_click(cancel.center(), Modifiers::none());
        settle(&mut harness);

        assert!(!shows(&mut harness, "BASE_UI_ALERT"));
        assert_eq!(
            stored_account(&mut harness).as_ref(),
            Some(&ethereum_account())
        );
        assert_eq!(harness.source.count(|state| state.discards), 0);
        assert_eq!(harness.source.count(|state| state.revocations), 0);
    }

    #[gpui_kit::test]
    fn confirming_a_sign_out_should_end_the_session_and_forget_the_account(
        cx: &mut TestAppContext,
    ) {
        let mut harness = signed_in_harness(cx);
        let report = watch_sign_out_report(&harness.source);
        harness.cx.update(|window, cx| {
            harness
                .menu
                .update(cx, |menu, cx| menu.confirm_sign_out(window, cx));
        });
        harness.cx.run_until_parked();
        let confirm = harness
            .cx
            .debug_bounds("BASE_UI_ALERT_OK")
            .expect("the confirmation should offer a sign-out entry");

        harness
            .cx
            .simulate_click(confirm.center(), Modifiers::none());
        settle(&mut harness);

        assert_eq!(harness.source.count(|state| state.discards), 1);
        assert!(stored_account(&mut harness).is_none());
        let _ = wait_for_sign_out_report(report);
        assert_eq!(
            harness.source.count(|state| state.revocations),
            1,
            "the service is told about the sign-out it did not have to allow"
        );
    }

    /// Reporting the sign-out is a request to the account service, and the executor this
    /// menu runs on is not a Tokio runtime: a report awaited there panics inside the
    /// request instead of being sent, which is what signing out used to do.
    #[gpui_kit::test]
    fn a_sign_out_should_report_it_on_the_application_runtime(cx: &mut TestAppContext) {
        let harness = signed_in_harness(cx);
        let report = watch_sign_out_report(&harness.source);

        harness.cx.update(|_, cx| {
            harness.menu.update(cx, |menu, cx| menu.sign_out(cx));
        });
        harness.cx.run_until_parked();

        assert!(
            wait_for_sign_out_report(report),
            "the sign-out report has to be driven by the application runtime"
        );
    }

    #[gpui_kit::test]
    fn a_confirmed_sign_out_should_forget_the_account_without_the_service(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);
        // The session has nothing left to hand over: no token, so nothing to report.
        harness.source.with(|state| state.revocation = None);

        harness.cx.update(|_, cx| {
            harness.menu.update(cx, |menu, cx| menu.sign_out(cx));
        });
        harness.cx.run_until_parked();
        open_menu(&mut harness);

        assert_eq!(harness.source.count(|state| state.discards), 1);
        assert!(stored_account(&mut harness).is_none());
        assert_eq!(harness.source.count(|state| state.revocations), 0);
        assert!(
            !shows(&mut harness, "BASE_UI_ALERT"),
            "a sign-out reports nothing: there is no failure left for it to report"
        );
    }

    #[gpui_kit::test]
    fn a_credential_that_is_no_longer_valid_should_forget_the_account(cx: &mut TestAppContext) {
        let source = FakeAccountService::new(ethereum_account());
        source.with(|state| state.local = Some(SessionAvailability::Unavailable));
        let mut harness = harness(cx, configured(ethereum_account()), source);
        harness.cx.run_until_parked();

        assert!(stored_account(&mut harness).is_none());
    }

    #[gpui_kit::test]
    fn a_credential_query_that_fails_should_keep_the_account(cx: &mut TestAppContext) {
        let source = FakeAccountService::new(ethereum_account());
        source.with(|state| state.local = None);
        let mut harness = harness(cx, configured(ethereum_account()), source);
        harness.cx.run_until_parked();

        assert_eq!(
            stored_account(&mut harness).as_ref(),
            Some(&ethereum_account())
        );
    }

    #[gpui_kit::test]
    fn the_tier_of_the_signed_in_account_should_be_read_and_kept(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);

        assert_eq!(menu_plan(&mut harness), Some(PlanCode::Plus));
    }

    #[gpui_kit::test]
    fn switching_accounts_should_read_the_tier_of_the_new_one(cx: &mut TestAppContext) {
        let mut harness = signed_in_harness(cx);
        assert_eq!(menu_plan(&mut harness), Some(PlanCode::Plus));

        harness
            .source
            .with(|state| state.plan = Some(Some(PlanCode::Free)));
        harness.cx.update(|_, cx| {
            cx.update_global::<ConfigManager, _>(|manager, _| {
                manager.update(|config| {
                    config.account = Some(other_account());
                });
            });
        });
        harness.cx.run_until_parked();

        assert_eq!(menu_plan(&mut harness), Some(PlanCode::Free));
        assert_eq!(
            menu_account(&mut harness).map(|account| account.wallet_address),
            Some(SOLANA.to_owned())
        );
    }

    #[gpui_kit::test]
    fn a_tier_that_could_not_be_read_should_leave_the_menu_saying_it_is_unknown(
        cx: &mut TestAppContext,
    ) {
        let source = FakeAccountService::new(ethereum_account());
        source.with(|state| state.plan_step = Step::Fail);
        let mut harness = harness(cx, configured(ethereum_account()), source);
        harness.cx.run_until_parked();

        assert_eq!(menu_plan(&mut harness), None);
    }

    #[gpui_kit::test]
    fn a_stored_avatar_should_be_decoded_for_the_trigger(cx: &mut TestAppContext) {
        let mut config = configured(ethereum_account());
        config.avatar =
            Some(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"></svg>"#.to_owned());
        let harness = harness(cx, config, FakeAccountService::new(ethereum_account()));

        assert!(
            harness
                .cx
                .update(|_, cx| harness.menu.read(cx).avatar.is_some())
        );
    }

    /// A missing key translates to an empty string, which is how this feature would fail
    /// quietly: a menu entry with no label, an alert that says nothing, a button that
    /// cannot be read. Every key the crate reads is listed here so a rename or a
    /// forgotten locale fails a test instead of blanking out the interface.
    #[gpui_kit::test]
    fn every_key_this_feature_reads_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });
        let keys = [
            "window.account.authorization_complete",
            "window.account.authorization_complete_message",
            "window.account.authorized_wallet",
            "window.account.awaiting_authorization",
            "window.account.cancel_sign_in",
            "window.account.copy_wallet_address",
            "window.account.menu",
            "window.account.plan_unavailable",
            "window.account.sign_in",
            "window.account.sign_out",
            "window.account.sign_out_confirm",
            "common.cancel",
            "common.confirm",
            "error.account.credential_store",
            "error.account.invalid_credential",
            "error.account.invalid_grant",
            "error.account.login_callback",
            "error.account.login_expired",
            "error.account.login_listener",
            "error.account.missing_credential",
            "error.account.refresh_unknown",
            "error.account.request",
            "error.account.response",
            "error.account.route",
            "error.account.secure_random",
            "error.account.sequence_exhausted",
            "error.account.session_closed",
            "error.account.status",
            "error.account.system_time",
        ];

        cx.update(|cx| {
            for key in keys {
                assert!(!i18n::t!(cx, key).is_empty(), "{key} has no copy");
            }

            // Every failure the account service can report has to say something. The
            // variants that take a payload this crate cannot build (`Status` carries a
            // `reqwest` status code, the rest carry an `io`/`rand`/`url` error) are
            // covered by the key list above.
            for error in [
                AuthError::CredentialStore,
                AuthError::MissingCredential,
                AuthError::InvalidCredential,
                AuthError::HttpClient,
                AuthError::Route,
                AuthError::Request,
                AuthError::RefreshOutcomeUnknown,
                AuthError::InvalidResponse,
                AuthError::InvalidGrant,
                AuthError::SystemTime,
                AuthError::SecureRandom,
                AuthError::SessionClosed,
                AuthError::SequenceExhausted,
            ] {
                assert!(
                    !super::oauth::service_failure(&error, cx).is_empty(),
                    "{error:?} has no copy"
                );
            }
            for error in [LoginError::Expired, LoginError::InvalidCallback] {
                assert!(!error.message(cx).is_empty(), "{error:?} has no copy");
            }
        });
    }

    /// The account a sign-in just authorized is reported in the middle of its alert, which
    /// is what the message says about itself with a `<p align="center">` tag pair.
    ///
    /// The pair has to be the whole of the message rather than wrapped around it here: the
    /// message is copy, so it is written in a locale, and the paragraph it is drawn in is as
    /// wide as the alert whether or not the text inside it is centred — which is why what is
    /// read here is the copy and not where it was laid out.
    ///
    /// Both languages are asked, and the account is asked for through placeholders that are
    /// words of their own, so a locale that dropped one of them fails here rather than
    /// showing the user a report about no account.
    #[gpui_kit::test]
    fn the_authorization_report_should_be_centred_copy_in_both_languages(cx: &mut TestAppContext) {
        use i18n::language::codes::LanguageCode;

        const KEY: &str = "window.account.authorization_complete_message";

        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });

        let account = ethereum_account();
        let kind = super::wallet_kind_label(account.wallet_kind);
        let wallet = SmolStr::new_static("the-wallet");
        let family = SmolStr::new_static("the-family");

        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let (message, filling) = cx.update(|cx| {
                cx.set_global(language);

                (
                    i18n::t!(cx, KEY, wallet = account.wallet_address, kind = kind),
                    i18n::t!(cx, KEY, wallet = wallet, kind = family),
                )
            });

            assert!(
                message.starts_with("<p align=\"center\">") && message.ends_with("</p>"),
                "the report of {language:?} should be centred copy: {message}"
            );
            assert!(
                filling.contains("the-family") && filling.contains("the-wallet"),
                "the report of {language:?} should write the account where its placeholders \
                 are: {filling}"
            );
            assert!(
                !message.contains("%{"),
                "the report of {language:?} should have no unread placeholder: {message}"
            );
            assert!(
                message.contains(&account.wallet_address) && message.contains(kind),
                "the report of {language:?} should carry the account it is about: {message}"
            );
        }
    }

    /// Starts a sign-in and lets the alert it opens be drawn.
    fn start_sign_in(harness: &mut Harness) {
        harness.cx.update(|window, cx| {
            harness
                .menu
                .update(cx, |menu, cx| menu.start_sign_in(window, cx));
        });
        harness.cx.run_until_parked();
    }

    /// Accepts the end of the attempt in progress, the way the task waiting for the browser
    /// does: on the foreground executor, reaching the window through its handle rather than
    /// through a `Window` the caller is already holding.
    ///
    /// That distinction is what `finish_sign_in` is built around, and calling it straight
    /// from inside a window update would re-enter one — which GPUI answers with "window not
    /// found" rather than with an account.
    fn finish_sign_in(harness: &mut Harness, report: Option<Result<AccountConfig, LoginError>>) {
        let handle = harness.cx.update(|window, _| window.window_handle());

        harness
            .cx
            .update_window(handle, |_, _, cx| {
                harness.menu.update(cx, |menu, cx| {
                    menu.finish_sign_in(report, handle, cx);
                });
            })
            .expect("the harness window should be open");
        settle(harness);
    }

    /// Whether the menu is still waiting for a browser callback.
    fn pending_sign_in(harness: &mut Harness) -> bool {
        harness
            .cx
            .update(|_, cx| harness.menu.read(cx).sign_in.is_some())
    }

    fn menu_plan(harness: &mut Harness) -> Option<PlanCode> {
        harness.cx.update(|_, cx| harness.menu.read(cx).plan)
    }

    fn menu_account(harness: &mut Harness) -> Option<AccountConfig> {
        harness
            .cx
            .update(|_, cx| harness.menu.read(cx).account.clone())
    }
}
