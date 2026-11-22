//! The host this crate's tests are driven against.
//!
//! The account control is an application-layer component, so the contract layer's
//! `WidgetTestApp` (built around `WidgetView`) does not fit it. This harness composes the same
//! aggregate the application root composes — the frozen [`WidgetServices`], the layer stack, the
//! frame router and the animation router — over `ui_core::test_support::TestApp`, and adds the
//! four things a control test needs: the two seams ([`FakeAccountService`] /
//! [`MemoryAccountSettings`]), an engine frame that runs the control's `sync` the way the shell's
//! frame route does, and the async turns that let the control's background flows finish.
//!
//! # The two seams
//!
//! [`FakeAccountService`] is the account service's OAuth session, with every step switchable, so a
//! rejected refresh, an account that switched mid-query and a sign-out that failed can each be
//! produced on demand.
//!
//! [`MemoryAccountSettings`] is the **executable form of the [`AccountSettings`] contract**: it
//! is the store the composition root owns (P4-3), written the way the contract states — a new
//! account drops the old face, clearing removes both, and a generated face is only stored for the
//! account it was generated for. The root's implementation has to satisfy the same four rules.
//!
//! # Frames
//!
//! [`Harness::drain`] runs one production-shaped turn: the runtime turns that finish the
//! control's spawned work, then the control's own frame route (`sync`), then one engine frame —
//! the order the platform pump uses. `TestApp::drain` never runs `App::on_frame`, which is why
//! the control's route is explicit here; the shell (P4-3) owns that route in production.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use app_config::settings::{AccountConfig, AccountWalletKind, Config};
use base_ui::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
};
use futures::future::BoxFuture;
use i18n::{language::codes::LanguageCode, translate::Translate};
use kytrix_service_provider::auth::{
    dpop::DpopKey,
    model::{AuthError, PlanCode, SessionAvailability, SessionChange},
    session::Revocation,
};
use smol_str::SmolStr;
use tokio::sync::watch;
use ui_core::{
    app::App,
    error::EngineError,
    event::{InputEvent, Modifiers, PointerButton},
    math::{Point, Rect, Size},
    node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
    test_support::{app::TestApp, diagnostics::SignalSlotStats},
    window::{WindowId, WindowOptions},
};
use widget::{frame::FrameRouter, services::WidgetServices};
use zeroize::Zeroizing;

use crate::{
    host::{AccountService, AccountSettings, AccountSnapshot, ProfileHost},
    oauth::LoginError,
    profile::ProfileMenu,
    session::LocalSession,
};

/// The wallet the fixtures sign in with.
pub(crate) const ETHEREUM: &str = "0x0123456789abcdef0123456789abcdef01234567";

/// A second wallet, which is what makes an account *change* observable.
pub(crate) const SOLANA: &str = "7YttLkHDoNj9wyDur5VjZ9Pd7WZZvnGvWKbxzwfKQnDb";

/// The one-time code the dashboard handoff fixture answers with.
pub(crate) const HANDOFF_CODE: &str = "0123456789012345678901234567890123456789012";

/// A generated avatar small enough to rasterize in a test.
pub(crate) const AVATAR_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#ff0000"/></svg>"##;

/// The failure every switchable step reports when it is set to fail.
pub(crate) const FAILURE: AuthError = AuthError::MissingCredential;

/// The translation context a test renders through: the product's own catalogue, so a test
/// asserts the sentence the user reads rather than a fixture of this crate's making.
pub(crate) fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
        language,
    )
}

/// An Ethereum account signed in as the fixture user.
pub(crate) fn ethereum_account() -> AccountConfig {
    AccountConfig {
        user_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        wallet_address: ETHEREUM.to_owned(),
        wallet_kind: AccountWalletKind::Ethereum,
    }
}

/// The other account, which is what makes an account change observable.
pub(crate) fn other_account() -> AccountConfig {
    AccountConfig {
        user_id: "22222222-2222-4222-8222-222222222222".to_owned(),
        wallet_address: SOLANA.to_owned(),
        wallet_kind: AccountWalletKind::Solana,
    }
}

/// A settings snapshot holding `account`, with no avatar generated yet.
pub(crate) fn configured(account: AccountConfig) -> AccountSnapshot {
    AccountSnapshot {
        account: Some(account),
        avatar: None,
    }
}

/// Whether one switchable step of the fake account service succeeds or fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Step {
    /// The step answers with its fixture.
    Succeed,
    /// The step answers with [`FAILURE`].
    Fail,
}

/// Everything the fake account service answers with, and what it was asked.
pub(crate) struct FakeState {
    pub prepare: Step,
    pub exchange: Step,
    pub plan: Option<Option<PlanCode>>,
    pub plan_step: Step,
    pub handoff: Step,
    pub handoff_code: String,
    /// What the credential on disk says; `None` is a query that failed.
    pub local: Option<SessionAvailability>,
    /// What the session hands over when it is signed out.
    pub revocation: Option<Revocation>,
    /// Where the next sign-out report says whether a runtime drove it. A test that waits on it
    /// knows the report was made, which the counters alone cannot say: a counter moves when a
    /// future is asked for, not when it is driven.
    pub revoke_report: Option<std::sync::mpsc::Sender<bool>>,
    pub prepared: usize,
    pub exchanges: usize,
    pub discards: usize,
    pub revocations: usize,
    pub handoffs: usize,
    pub plans: usize,
}

impl FakeState {
    fn new() -> Self {
        Self {
            prepare: Step::Succeed,
            exchange: Step::Succeed,
            plan: Some(Some(PlanCode::Plus)),
            plan_step: Step::Succeed,
            handoff: Step::Succeed,
            handoff_code: HANDOFF_CODE.to_owned(),
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
            handoffs: 0,
            plans: 0,
        }
    }
}

/// The account service's OAuth session, with every step switchable.
#[derive(Clone)]
pub(crate) struct FakeAccountService {
    state: Arc<Mutex<FakeState>>,
    changes: watch::Sender<SessionChange>,
    exchanged: AccountConfig,
}

impl FakeAccountService {
    /// A service whose authorization exchanges into `exchanged`.
    pub(crate) fn new(exchanged: AccountConfig) -> Self {
        let (changes, _) = watch::channel(SessionChange::default());

        Self {
            state: Arc::new(Mutex::new(FakeState::new())),
            changes,
            exchanged,
        }
    }

    /// Runs `update` against the fake's state.
    pub(crate) fn with<R>(&self, update: impl FnOnce(&mut FakeState) -> R) -> R {
        update(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Counts something in the fake's state (how often a step was asked).
    pub(crate) fn count(&self, count: impl FnOnce(&mut FakeState) -> usize) -> usize {
        self.with(count)
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
        _code_verifier: Zeroizing<String>,
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
                // The production report is a request to the account service, and it only runs if
                // something drives it on a tokio runtime: asking here is what tells the
                // difference apart.
                let _ = report.send(tokio::runtime::Handle::try_current().is_ok());
            }
        })
    }

    fn current_plan<'a>(
        &'a self,
        _user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<PlanCode>, AuthError>> {
        let result = self.with(|state| {
            state.plans += 1;

            match state.plan_step {
                Step::Succeed => Ok(state.plan.flatten()),
                Step::Fail => Err(FAILURE),
            }
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

    fn create_dashboard_handoff(&self) -> BoxFuture<'_, Result<String, AuthError>> {
        let result = self.with(|state| {
            state.handoffs += 1;

            match state.handoff {
                Step::Succeed => Ok(state.handoff_code.clone()),
                Step::Fail => Err(FAILURE),
            }
        });

        Box::pin(async move { result })
    }

    fn session_changes(&self) -> watch::Receiver<SessionChange> {
        self.changes.subscribe()
    }
}

/// The account settings store: the executable form of the [`AccountSettings`] contract.
///
/// It is also the composition root's other job in miniature: every write republishes the settings
/// signal, which is what asks the control for the frame that reads the new record.
pub(crate) struct MemoryAccountSettings {
    snapshot: RefCell<AccountSnapshot>,
    config: ui_core::node::signal::Signal<Config>,
}

impl MemoryAccountSettings {
    /// A store holding `snapshot`, publishing into the settings signal the services carry.
    ///
    /// The signal is **the one inside [`WidgetServices`]** (the aggregate mints it), because that
    /// is the signal the composition root keeps in step with its store and the one the control
    /// watches; a store with a signal of its own would write news nobody reads.
    pub(crate) fn new(
        app: &mut App,
        snapshot: AccountSnapshot,
        config: ui_core::node::signal::Signal<Config>,
    ) -> Self {
        let mut value = app.read(&config).clone();
        value.account = snapshot.account.clone();
        value.avatar = snapshot.avatar.as_ref().map(ToString::to_string);
        app.write(&config, value);

        Self {
            snapshot: RefCell::new(snapshot),
            config,
        }
    }

    /// The settings signal the root publishes on every write.
    pub(crate) fn config(&self) -> ui_core::node::signal::Signal<Config> {
        self.config
    }

    /// Republishes the settings snapshot the signal carries.
    fn publish(&self, app: &mut App) {
        let snapshot = self.snapshot.borrow().clone();
        let mut config = app.read(&self.config).clone();
        config.account = snapshot.account;
        config.avatar = snapshot.avatar.map(|svg| svg.to_string());
        app.write(&self.config, config);
    }
}

impl AccountSettings for MemoryAccountSettings {
    fn read(&self, _app: &App) -> AccountSnapshot {
        self.snapshot.borrow().clone()
    }

    fn store_account(&self, app: &mut App, account: AccountConfig) {
        {
            let mut snapshot = self.snapshot.borrow_mut();
            snapshot.account = Some(account);
            // The face belongs to the account that just went away.
            snapshot.avatar = None;
        }
        self.publish(app);
    }

    fn clear_account(&self, app: &mut App) {
        {
            let mut snapshot = self.snapshot.borrow_mut();
            snapshot.account = None;
            snapshot.avatar = None;
        }
        self.publish(app);
    }

    fn store_avatar_if_current(&self, app: &mut App, wallet: &str, svg: String) -> bool {
        {
            let mut snapshot = self.snapshot.borrow_mut();
            let current = snapshot
                .account
                .as_ref()
                .is_some_and(|account| account.wallet_address == wallet);
            if !current {
                return false;
            }
            snapshot.avatar = Some(SmolStr::new(svg));
        }
        self.publish(app);

        true
    }
}

/// The headless host every control test runs against.
pub(crate) struct Harness {
    app: TestApp,
    /// The runtime the control's long-running work is handed (the application's own, in
    /// production).
    runtime: tokio::runtime::Runtime,
    services: Rc<WidgetServices>,
    router: Rc<FrameRouter>,
    accounts: FakeAccountService,
    settings: Rc<MemoryAccountSettings>,
    menu: ProfileMenu,
    parent: NodeId,
    opened_urls: Rc<RefCell<Vec<SmolStr>>>,
}

impl Harness {
    /// A control whose settings hold nothing: nobody is signed in.
    pub(crate) fn new() -> Self {
        Self::with(
            AccountSnapshot::default(),
            FakeAccountService::new(ethereum_account()),
        )
    }

    /// A control whose settings hold the fixture account.
    pub(crate) fn signed_in() -> Self {
        Self::with(
            configured(ethereum_account()),
            FakeAccountService::new(ethereum_account()),
        )
    }

    /// A control over `snapshot` whose account service is `accounts`.
    pub(crate) fn with(snapshot: AccountSnapshot, accounts: FakeAccountService) -> Self {
        let mut app = TestApp::with_options(WindowOptions {
            size: Size::new(1280.0, 900.0),
            ..WindowOptions::default()
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the test runtime");
        // The same aggregate and the same three composition-root singletons the application root
        // builds: the control reaches none of them through a global of its own.
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let layer = Rc::new(Layer::new());
        let router = Rc::new(FrameRouter::new(app.app_mut()).expect("the test host owns the hook"));
        let animation = Rc::new(AnimationRouter::new());
        let opened_urls = Rc::new(RefCell::new(Vec::new()));
        let opened = Rc::clone(&opened_urls);
        let window = app.window_id();
        let services = Rc::new(
            WidgetServices::builder(
                app.app_mut(),
                runtime.handle().clone(),
                translate(LanguageCode::En),
                theme,
                Rc::clone(&layer),
                Rc::clone(&router),
                Rc::clone(&animation),
            )
            .open_url(move |url| opened.borrow_mut().push(SmolStr::new(url)))
            .build(),
        );
        // The store publishes into the aggregate's own settings signal — the one the composition
        // root keeps in step and the control watches.
        let settings = Rc::new(MemoryAccountSettings::new(
            app.app_mut(),
            snapshot,
            services.config,
        ));
        let host = ProfileHost::new(
            Rc::clone(&services),
            Arc::new(accounts.clone()),
            Rc::clone(&settings) as Rc<dyn AccountSettings>,
        );
        let root = app
            .app()
            .window(window)
            .expect("the harness window")
            .tree()
            .root();
        let parent = NodeBuilder::boxed()
            .style(
                base_ui::style_builder::StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .padding(8.0)
                    .build(),
            )
            .build(
                app.app_mut()
                    .window_mut(window)
                    .expect("the harness window")
                    .tree_mut(),
                root,
            )
            .expect("the harness slot");
        let mut menu = ProfileMenu::new(host);
        menu.build(app.app_mut(), window, parent)
            .expect("the control builds");
        let mut harness = Self {
            app,
            runtime,
            services,
            router,
            accounts,
            settings,
            menu,
            parent,
            opened_urls,
        };
        // A signed-in control generates its face on the blocking pool, and the frame that accepts
        // it rebuilds what draws the face (the trigger and, when open, the menu). Every test
        // therefore starts from the same place: the face has landed, or there is no account to
        // generate one for. A test that watches the generation itself asks for a harness whose
        // account has no face yet and waits for it (`settle_until`), which is the same path.
        harness.settle(2);
        harness.settle_until(200, |harness| {
            harness.menu().face().is_some() || harness.menu().account().is_none()
        });
        harness.settle(2);

        harness
    }

    /// The control under test.
    pub(crate) fn menu(&self) -> &ProfileMenu {
        &self.menu
    }

    /// The account service seam, for a test that switches one of its steps.
    pub(crate) fn accounts(&self) -> &FakeAccountService {
        &self.accounts
    }

    /// The settings store, for a test that writes the account the way the root does.
    pub(crate) fn settings(&self) -> &Rc<MemoryAccountSettings> {
        &self.settings
    }

    /// The host the control was built with.
    pub(crate) fn host(&self) -> ProfileHost {
        ProfileHost::new(
            Rc::clone(&self.services),
            Arc::new(self.accounts.clone()),
            Rc::clone(&self.settings) as Rc<dyn AccountSettings>,
        )
    }

    /// The URLs the control asked the application to open.
    pub(crate) fn opened_urls(&self) -> Vec<SmolStr> {
        self.opened_urls.borrow().clone()
    }

    /// The last URL the control asked the application to open.
    pub(crate) fn opened_url(&self) -> Option<SmolStr> {
        self.opened_urls.borrow().last().cloned()
    }

    pub(crate) fn app(&self) -> &App {
        self.app.app()
    }

    pub(crate) fn window(&self) -> WindowId {
        self.app.window_id()
    }

    pub(crate) fn root(&self) -> NodeId {
        self.app
            .app()
            .window(self.app.window_id())
            .expect("the harness window exists")
            .tree()
            .root()
    }

    /// The slot the control was mounted into.
    pub(crate) fn parent(&self) -> NodeId {
        self.parent
    }

    /// Mounts a second control in the same application (the same `App`, the same services).
    ///
    /// The slot it is mounted into is returned with it: the caller owns that slot, so removing it
    /// is the caller's half of the teardown order (E16: nodes before the control's release).
    pub(crate) fn mount_second(&mut self) -> (ProfileMenu, NodeId) {
        let window = self.app.window_id();
        let root = self.root();
        let slot = NodeBuilder::boxed()
            .style(
                base_ui::style_builder::StyleBuilder::new()
                    .flex_row()
                    .build(),
            )
            .build(
                self.app
                    .app_mut()
                    .window_mut(window)
                    .expect("the harness window")
                    .tree_mut(),
                root,
            )
            .expect("the second slot");
        let mut menu = ProfileMenu::new(self.host());
        menu.build(self.app.app_mut(), window, slot)
            .expect("the second control builds");

        (menu, slot)
    }

    /// One production-shaped turn: the runtime turns, the control's route, and one engine frame.
    pub(crate) fn drain(&mut self) {
        self.drain_runtime();
        let _ = self.menu.sync(self.app.app_mut());
        let _ = self.app.drain();
    }

    /// Turns the runtime one step, which is how the control's spawned work finishes.
    ///
    /// The control submits its flows (and its blocking work) to the injected
    /// [`WidgetServices::runtime`]; the harness owns that runtime, so one turn here is enough to
    /// run whatever the control asked for, and nothing sleeps on a wall clock.
    pub(crate) fn drain_runtime(&mut self) {
        self.runtime.block_on(tokio::task::yield_now());
    }

    /// Runs `frames` production-shaped turns.
    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

    /// Runs turns until `ready` holds, or `attempts` turns passed; returns whether it held.
    ///
    /// Some of the control's work finishes on another thread (the rasterizer, the blocking pool),
    /// so a test that waits for it gives it a few turns and a little wall time rather than a
    /// fixed count. The predicate is asked at the top of a turn, so a condition that already
    /// holds costs one turn.
    pub(crate) fn settle_until(
        &mut self,
        attempts: usize,
        mut ready: impl FnMut(&Harness) -> bool,
    ) -> bool {
        for _ in 0..attempts {
            if ready(self) {
                return true;
            }
            self.drain();
            std::thread::sleep(Duration::from_millis(2));
        }
        ready(self)
    }

    /// Advances the engine clock (one-shot timers and the overlays' fades are measured against
    /// it), then runs the two production-shaped turns the advance needs.
    ///
    /// `TestApp::advance` only moves the clock: a due timer is dispatched at the **frame entry**
    /// of the next frame, and what its callback wrote is applied by the frame route of the turn
    /// after that. A test that advances and then reads a state the timer drives therefore needs
    /// both turns, which is what this method runs.
    pub(crate) fn advance(&mut self, by: Duration) {
        self.app.advance(by);
        self.drain();
        self.drain();
    }

    /// The layout box of one node, if it is laid out.
    pub(crate) fn bounds(&self, node: NodeId) -> Option<Rect> {
        self.app.probe().bounds(node)
    }

    /// Moves the pointer over one window position, as the platform pump reports motion.
    pub(crate) fn move_pointer(&mut self, position: Point) {
        let _ = self.app.move_pointer(position);
    }

    /// Clicks one node at the centre of its layout box.
    pub(crate) fn click_node(&mut self, node: NodeId) {
        let bounds = self
            .bounds(node)
            .unwrap_or_else(|| panic!("the node is not laid out; ids: {:?}", self.debug_ids()));
        let centre = Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        self.move_pointer(centre);
        self.click_at(centre);
    }

    /// Clicks one window position with the primary button.
    pub(crate) fn click_at(&mut self, position: Point) {
        self.app.inject(InputEvent::pointer_down(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        self.app.inject(InputEvent::pointer_up(
            position,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    /// Clicks the control's trigger.
    pub(crate) fn click_trigger(&mut self) {
        let node = self
            .menu
            .trigger_node()
            .expect("the control built its trigger");
        self.click_node(node);
    }

    /// Opens the account menu the way a press does, and lets it settle.
    pub(crate) fn open_menu(&mut self) {
        self.click_trigger();
        self.settle(3);
    }

    /// Clicks the confirming button of the alert that is up.
    pub(crate) fn click_alert_confirm(&mut self) {
        let node = self
            .menu
            .alert_confirm()
            .and_then(|button| button.node())
            .expect("the alert draws its confirming button");
        self.click_node(node);
        self.settle(2);
    }

    /// Clicks the declining button of the alert that is up.
    pub(crate) fn click_alert_cancel(&mut self) {
        let node = self
            .menu
            .alert_cancel()
            .and_then(|button| button.node())
            .expect("the alert draws its declining button");
        self.click_node(node);
        self.settle(2);
    }

    /// Starts a sign-in from the control, the way a shell command might (no trigger press).
    pub(crate) fn start_sign_in(&mut self) {
        let Harness { app, menu, .. } = self;
        menu.start_sign_in_for_test(app.app_mut())
            .expect("the sign-in starts");
        self.settle(2);
    }

    /// Asks the sign-out question from the control (no menu entry press).
    pub(crate) fn confirm_sign_out(&mut self) {
        let Harness { app, menu, .. } = self;
        menu.confirm_sign_out_for_test(app.app_mut())
            .expect("the question opens");
        self.settle(2);
    }

    /// Signs out from the control, the way the confirmed question does.
    pub(crate) fn sign_out(&mut self) {
        let Harness { app, menu, .. } = self;
        menu.sign_out_for_test(app.app_mut())
            .expect("the sign-out runs");
        self.settle(2);
    }

    /// Delivers the end of a sign-in attempt, the way the task waiting on the browser does.
    pub(crate) fn finish_sign_in(&mut self, outcome: Option<Result<AccountConfig, LoginError>>) {
        let Harness { app, menu, .. } = self;
        menu.finish_sign_in_for_test(app.app_mut(), outcome);
        self.settle(2);
    }

    /// Waits for a watched sign-out report, and says whether a runtime drove it.
    pub(crate) fn wait_for_sign_out_report(observed: std::sync::mpsc::Receiver<bool>) -> bool {
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("the sign-out report should be made before the deadline")
    }

    /// Watches the report the next sign-out makes to the account service.
    pub(crate) fn watch_sign_out_report(&self) -> std::sync::mpsc::Receiver<bool> {
        let (report, observed) = std::sync::mpsc::channel();
        self.accounts
            .with(|state| state.revoke_report = Some(report));

        observed
    }

    /// Builds a control into this harness's window (a second control, or the one under test).
    pub(crate) fn build_control(
        &mut self,
        menu: &mut ProfileMenu,
        window: WindowId,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let Harness { app, .. } = self;
        menu.build(app.app_mut(), window, parent)
    }

    /// Releases a control against this harness's application.
    pub(crate) fn release_control(&mut self, menu: &mut ProfileMenu) {
        let Harness { app, .. } = self;
        menu.release(app.app_mut());
    }

    /// Removes one node of this harness's window (the caller's half of E16).
    pub(crate) fn remove_node(&mut self, node: NodeId) -> Result<(), EngineError> {
        let Harness { app, .. } = self;
        let window = app.window_id();
        app.app_mut().remove_node(window, node).map(|_| ())
    }

    /// Builds the control under test again (a second `build` is a no-op by contract).
    pub(crate) fn rebuild_control(
        &mut self,
        window: WindowId,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let Harness { app, menu, .. } = self;
        menu.build(app.app_mut(), window, parent)
    }

    /// Releases the control under test against this harness's application.
    pub(crate) fn release_menu(&mut self) {
        let Harness { app, menu, .. } = self;
        menu.release(app.app_mut());
    }

    /// Hovers the control's trigger.
    pub(crate) fn hover_trigger(&mut self) {
        let node = self
            .menu
            .trigger_node()
            .expect("the control built its trigger");
        if let Some(bounds) = self.bounds(node) {
            self.move_pointer(Point::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            ));
        }
    }

    /// The first node under the window root whose debug id starts with `prefix`.
    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        self.find_all(prefix).into_iter().next()
    }

    /// Every node under the window root whose debug id starts with `prefix`, in DFS order.
    pub(crate) fn find_all(&self, prefix: &str) -> Vec<NodeId> {
        let Some(window) = self.app.app().window(self.app.window_id()) else {
            return Vec::new();
        };
        let tree = window.tree();
        let mut found = Vec::new();
        let mut stack = vec![tree.root()];
        while let Some(node) = stack.pop() {
            if tree.debug_id(node).is_some_and(|id| id.starts_with(prefix)) {
                found.push(node);
            }
            if let Ok(entry) = tree.get(node) {
                stack.extend(entry.children().iter().rev().copied());
            }
        }
        found
    }

    /// Every debug id in the tree, in DFS order.
    pub(crate) fn debug_ids(&self) -> Vec<SmolStr> {
        let Some(window) = self.app.app().window(self.app.window_id()) else {
            return Vec::new();
        };
        let tree = window.tree();
        let mut ids = Vec::new();
        let mut stack = vec![tree.root()];
        while let Some(node) = stack.pop() {
            if let Some(id) = tree.debug_id(node) {
                ids.push(SmolStr::new(id));
            }
            if let Ok(entry) = tree.get(node) {
                stack.extend(entry.children().iter().rev().copied());
            }
        }

        ids
    }

    /// The text the subtree under `node` draws, one line per text node, in DFS order.
    pub(crate) fn text_of(&self, node: NodeId) -> String {
        let mut lines = Vec::new();
        let Some(window) = self.app.app().window(self.app.window_id()) else {
            return String::new();
        };
        let tree = window.tree();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let Ok(entry) = tree.get(current) else {
                continue;
            };
            if let NodeKind::Text(content) = entry.kind() {
                lines.push(content.text.clone());
            }
            stack.extend(entry.children().iter().rev().copied());
        }

        lines.join("\n")
    }

    /// Whether the subtree draws `text` anywhere.
    pub(crate) fn has_text(&self, node: NodeId, text: &str) -> bool {
        self.text_of(node).lines().any(|line| line == text)
    }

    /// The text one text node draws.
    pub(crate) fn node_text(&self, node: NodeId) -> SmolStr {
        let tree = self
            .app
            .app()
            .window(self.app.window_id())
            .expect("the harness window exists")
            .tree();
        match tree.get(node).expect("the node exists").kind() {
            NodeKind::Text(content) => content.text.clone(),
            other => panic!("expected a text node, got {other:?}"),
        }
    }

    /// The number of nodes in the harness window's tree (a released control leaves none behind).
    pub(crate) fn tree_len(&self) -> usize {
        self.app
            .app()
            .window(self.app.window_id())
            .expect("the harness window exists")
            .tree()
            .len()
    }

    /// The dialogs and menus open in this window.
    pub(crate) fn overlay_count(&self) -> usize {
        self.app.app().overlays(self.app.window_id()).len()
    }

    /// The engine's committed-frame request counter (the I3 assertion面).
    pub(crate) fn frame_requests(&self) -> u64 {
        self.app.app().frame_requests()
    }

    /// The live / allocated value slots (the control's release must give them back).
    pub(crate) fn signal_slots(&self) -> SignalSlotStats {
        ui_core::test_support::diagnostics::signal_slot_stats(self.app.app())
    }

    /// Switches the interface language the way the composition root does: the translator first,
    /// then the signal every `bind_text` closure reads.
    pub(crate) fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        let signal = self.services.language;
        self.app.app_mut().write(&signal, language);
    }

    /// Writes the account the way the composition root (or a sign-in) does.
    pub(crate) fn store_account(&mut self, account: AccountConfig) {
        self.settings.store_account(self.app.app_mut(), account);
    }

    /// Writes the account's avatar the way a completed generation does.
    pub(crate) fn store_avatar(&mut self, wallet: &str, svg: &str) -> bool {
        self.settings
            .store_avatar_if_current(self.app.app_mut(), wallet, svg.to_owned())
    }

    /// Forgets the account the way a sign-out does.
    pub(crate) fn clear_account(&mut self) {
        self.settings.clear_account(self.app.app_mut());
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The overlays and alerts are still mounted; one more frame drains their node work before
        // the application goes away with the router.
        let _ = self.router.tick(self.app.app_mut());
        self.app.drain().ok();
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::{AccountWalletKind, Config};
    use smol_str::SmolStr;
    use ui_core::test_support::app::TestApp;

    use super::{AccountSnapshot, ETHEREUM, MemoryAccountSettings, SOLANA, ethereum_account};
    use crate::host::AccountSettings;

    /// A store already holding an account and its generated face.
    fn holding_face(app: &mut TestApp) -> MemoryAccountSettings {
        let config = app.app_mut().signal(Config::default());

        MemoryAccountSettings::new(
            app.app_mut(),
            AccountSnapshot {
                account: Some(ethereum_account()),
                avatar: Some(SmolStr::new_static("<svg>old</svg>")),
            },
            config,
        )
    }

    /// Rule 2: a new account replaces the record and drops the face of the account that went
    /// away — a menu that showed the new address beside the old face would show two accounts.
    #[test]
    fn storing_an_account_should_replace_the_identity_and_drop_the_old_avatar() {
        let mut app = TestApp::new();
        let settings = holding_face(&mut app);

        settings.store_account(app.app_mut(), super::other_account());

        let snapshot = settings.read(app.app());
        let stored = snapshot.account.expect("the new account is stored");
        assert_eq!(stored.wallet_address, SOLANA);
        assert_eq!(stored.wallet_kind, AccountWalletKind::Solana);
        assert_eq!(snapshot.avatar, None);
        assert_eq!(
            app.app().read(&settings.config()).account.as_ref(),
            Some(&stored),
            "a write republishes the settings signal the control follows"
        );
    }

    /// Rule 3: clearing removes the record and the face with it.
    #[test]
    fn clearing_an_account_should_remove_the_identity_and_the_avatar() {
        let mut app = TestApp::new();
        let settings = holding_face(&mut app);

        settings.clear_account(app.app_mut());

        let snapshot = settings.read(app.app());
        assert!(snapshot.account.is_none());
        assert!(snapshot.avatar.is_none());
        assert!(app.app().read(&settings.config()).account.is_none());
    }

    /// Rule 4: a generated face is only stored for the account it was generated for — generation
    /// is a background task, so it can finish after the user signed out or after a different
    /// wallet signed in.
    #[test]
    fn a_generated_avatar_should_only_be_stored_for_the_current_account() {
        let mut app = TestApp::new();
        let config = app.app_mut().signal(Config::default());
        let settings =
            MemoryAccountSettings::new(app.app_mut(), AccountSnapshot::default(), config);

        assert!(
            !settings.store_avatar_if_current(app.app_mut(), "wallet-a", "<svg>a</svg>".to_owned()),
            "nobody is signed in, so no face is stored"
        );

        settings.store_account(app.app_mut(), ethereum_account());
        assert!(
            !settings.store_avatar_if_current(app.app_mut(), "wallet-b", "<svg>b</svg>".to_owned()),
            "a face for another wallet is not this account's"
        );
        assert!(
            settings.store_avatar_if_current(app.app_mut(), ETHEREUM, "<svg>a</svg>".to_owned()),
            "the face of the signed-in wallet is stored"
        );

        assert_eq!(
            settings.read(app.app()).avatar.as_deref(),
            Some("<svg>a</svg>")
        );
    }

    /// A store whose record is gone keeps no readable face: `avatar_of` states the same rule on
    /// the snapshot, so a menu cannot draw a face for nobody even if a store kept one.
    #[test]
    fn a_snapshot_without_an_account_has_no_readable_avatar() {
        let mut app = TestApp::new();
        let config = app.app_mut().signal(Config::default());
        let settings = MemoryAccountSettings::new(
            app.app_mut(),
            AccountSnapshot {
                account: None,
                avatar: Some(SmolStr::new_static("<svg>orphan</svg>")),
            },
            config,
        );

        let snapshot = settings.read(app.app());

        assert_eq!(crate::account::avatar_of(&snapshot), None);
    }
}
