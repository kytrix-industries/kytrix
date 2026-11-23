use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use app_config::settings::{AccountConfig, AccountWalletKind, Config};
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
use ui_kit::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::{appearance::Appearance, handle::ThemeHandle},
};
use widget::{frame::FrameRouter, services::WidgetServices};
use zeroize::Zeroizing;

use crate::{
    host::{AccountService, AccountSettings, AccountSnapshot, ProfileHost},
    oauth::LoginError,
    profile::ProfileMenu,
    session::LocalSession,
};

pub(crate) const ETHEREUM: &str = "0x0123456789abcdef0123456789abcdef01234567";

pub(crate) const SOLANA: &str = "7YttLkHDoNj9wyDur5VjZ9Pd7WZZvnGvWKbxzwfKQnDb";

pub(crate) const HANDOFF_CODE: &str = "0123456789012345678901234567890123456789012";

pub(crate) const AVATAR_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#ff0000"/></svg>"##;

pub(crate) const FAILURE: AuthError = AuthError::MissingCredential;

pub(crate) fn translate(language: LanguageCode) -> Translate {
    Translate::new(
        Rc::new(i18n::embedded::load().expect("the embedded locales load")),
        language,
    )
}

pub(crate) fn ethereum_account() -> AccountConfig {
    AccountConfig {
        user_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        wallet_address: ETHEREUM.to_owned(),
        wallet_kind: AccountWalletKind::Ethereum,
    }
}

pub(crate) fn other_account() -> AccountConfig {
    AccountConfig {
        user_id: "22222222-2222-4222-8222-222222222222".to_owned(),
        wallet_address: SOLANA.to_owned(),
        wallet_kind: AccountWalletKind::Solana,
    }
}

pub(crate) fn configured(account: AccountConfig) -> AccountSnapshot {
    AccountSnapshot {
        account: Some(account),
        avatar: None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Step {
    Succeed,
    Fail,
}

pub(crate) struct FakeState {
    pub prepare: Step,
    pub exchange: Step,
    pub plan: Option<Option<PlanCode>>,
    pub plan_step: Step,
    pub handoff: Step,
    pub handoff_code: String,
    pub local: Option<SessionAvailability>,
    pub revocation: Option<Revocation>,
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

#[derive(Clone)]
pub(crate) struct FakeAccountService {
    state: Arc<Mutex<FakeState>>,
    changes: watch::Sender<SessionChange>,
    exchanged: AccountConfig,
}

impl FakeAccountService {
    pub(crate) fn new(exchanged: AccountConfig) -> Self {
        let (changes, _) = watch::channel(SessionChange::default());

        Self {
            state: Arc::new(Mutex::new(FakeState::new())),
            changes,
            exchanged,
        }
    }

    pub(crate) fn with<R>(&self, update: impl FnOnce(&mut FakeState) -> R) -> R {
        update(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

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

pub(crate) struct MemoryAccountSettings {
    snapshot: RefCell<AccountSnapshot>,
    config: ui_core::node::signal::Signal<Config>,
}

impl MemoryAccountSettings {
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

    pub(crate) fn config(&self) -> ui_core::node::signal::Signal<Config> {
        self.config
    }

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

pub(crate) struct Harness {
    app: TestApp,
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
    pub(crate) fn new() -> Self {
        Self::with(
            AccountSnapshot::default(),
            FakeAccountService::new(ethereum_account()),
        )
    }

    pub(crate) fn signed_in() -> Self {
        Self::with(
            configured(ethereum_account()),
            FakeAccountService::new(ethereum_account()),
        )
    }

    pub(crate) fn with(snapshot: AccountSnapshot, accounts: FakeAccountService) -> Self {
        let mut app = TestApp::with_options(WindowOptions {
            size: Size::new(1280.0, 900.0),
            ..WindowOptions::default()
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the test runtime");
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
                ui_kit::style_builder::StyleBuilder::new()
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
        harness.settle(2);
        harness.settle_until(200, |harness| {
            harness.menu().face().is_some() || harness.menu().account().is_none()
        });
        harness.settle(2);

        harness
    }

    pub(crate) fn menu(&self) -> &ProfileMenu {
        &self.menu
    }

    pub(crate) fn accounts(&self) -> &FakeAccountService {
        &self.accounts
    }

    pub(crate) fn settings(&self) -> &Rc<MemoryAccountSettings> {
        &self.settings
    }

    pub(crate) fn host(&self) -> ProfileHost {
        ProfileHost::new(
            Rc::clone(&self.services),
            Arc::new(self.accounts.clone()),
            Rc::clone(&self.settings) as Rc<dyn AccountSettings>,
        )
    }

    pub(crate) fn opened_urls(&self) -> Vec<SmolStr> {
        self.opened_urls.borrow().clone()
    }

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

    pub(crate) fn parent(&self) -> NodeId {
        self.parent
    }

    pub(crate) fn mount_second(&mut self) -> (ProfileMenu, NodeId) {
        let window = self.app.window_id();
        let root = self.root();
        let slot = NodeBuilder::boxed()
            .style(
                ui_kit::style_builder::StyleBuilder::new()
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

    pub(crate) fn drain(&mut self) {
        self.drain_runtime();
        let _ = self.menu.sync(self.app.app_mut());
        let _ = self.app.drain();
    }

    pub(crate) fn drain_runtime(&mut self) {
        self.runtime.block_on(tokio::task::yield_now());
    }

    pub(crate) fn settle(&mut self, frames: usize) {
        for _ in 0..frames {
            self.drain();
        }
    }

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

    pub(crate) fn advance(&mut self, by: Duration) {
        self.app.advance(by);
        self.drain();
        self.drain();
    }

    pub(crate) fn bounds(&self, node: NodeId) -> Option<Rect> {
        self.app.probe().bounds(node)
    }

    pub(crate) fn move_pointer(&mut self, position: Point) {
        let _ = self.app.move_pointer(position);
    }

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

    pub(crate) fn click_trigger(&mut self) {
        let node = self
            .menu
            .trigger_node()
            .expect("the control built its trigger");
        self.click_node(node);
    }

    pub(crate) fn open_menu(&mut self) {
        self.click_trigger();
        self.settle(3);
    }

    pub(crate) fn click_alert_confirm(&mut self) {
        let node = self
            .menu
            .alert_confirm()
            .and_then(|button| button.node())
            .expect("the alert draws its confirming button");
        self.click_node(node);
        self.settle(2);
    }

    pub(crate) fn click_alert_cancel(&mut self) {
        let node = self
            .menu
            .alert_cancel()
            .and_then(|button| button.node())
            .expect("the alert draws its declining button");
        self.click_node(node);
        self.settle(2);
    }

    pub(crate) fn start_sign_in(&mut self) {
        let Harness { app, menu, .. } = self;
        menu.start_sign_in_for_test(app.app_mut())
            .expect("the sign-in starts");
        self.settle(2);
    }

    pub(crate) fn confirm_sign_out(&mut self) {
        let Harness { app, menu, .. } = self;
        menu.confirm_sign_out_for_test(app.app_mut())
            .expect("the question opens");
        self.settle(2);
    }

    pub(crate) fn sign_out(&mut self) {
        let Harness { app, menu, .. } = self;
        menu.sign_out_for_test(app.app_mut())
            .expect("the sign-out runs");
        self.settle(2);
    }

    pub(crate) fn finish_sign_in(&mut self, outcome: Option<Result<AccountConfig, LoginError>>) {
        let Harness { app, menu, .. } = self;
        menu.finish_sign_in_for_test(app.app_mut(), outcome);
        self.settle(2);
    }

    pub(crate) fn wait_for_sign_out_report(observed: std::sync::mpsc::Receiver<bool>) -> bool {
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("the sign-out report should be made before the deadline")
    }

    pub(crate) fn watch_sign_out_report(&self) -> std::sync::mpsc::Receiver<bool> {
        let (report, observed) = std::sync::mpsc::channel();
        self.accounts
            .with(|state| state.revoke_report = Some(report));

        observed
    }

    pub(crate) fn build_control(
        &mut self,
        menu: &mut ProfileMenu,
        window: WindowId,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let Harness { app, .. } = self;
        menu.build(app.app_mut(), window, parent)
    }

    pub(crate) fn release_control(&mut self, menu: &mut ProfileMenu) {
        let Harness { app, .. } = self;
        menu.release(app.app_mut());
    }

    pub(crate) fn remove_node(&mut self, node: NodeId) -> Result<(), EngineError> {
        let Harness { app, .. } = self;
        let window = app.window_id();
        app.app_mut().remove_node(window, node).map(|_| ())
    }

    pub(crate) fn rebuild_control(
        &mut self,
        window: WindowId,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let Harness { app, menu, .. } = self;
        menu.build(app.app_mut(), window, parent)
    }

    pub(crate) fn release_menu(&mut self) {
        let Harness { app, menu, .. } = self;
        menu.release(app.app_mut());
    }

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

    pub(crate) fn find(&self, prefix: &str) -> Option<NodeId> {
        self.find_all(prefix).into_iter().next()
    }

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

    pub(crate) fn has_text(&self, node: NodeId, text: &str) -> bool {
        self.text_of(node).lines().any(|line| line == text)
    }

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

    pub(crate) fn tree_len(&self) -> usize {
        self.app
            .app()
            .window(self.app.window_id())
            .expect("the harness window exists")
            .tree()
            .len()
    }

    pub(crate) fn overlay_count(&self) -> usize {
        self.app.app().overlays(self.app.window_id()).len()
    }

    pub(crate) fn frame_requests(&self) -> u64 {
        self.app.app().frame_requests()
    }

    pub(crate) fn signal_slots(&self) -> SignalSlotStats {
        ui_core::test_support::diagnostics::signal_slot_stats(self.app.app())
    }

    pub(crate) fn switch_language(&mut self, language: LanguageCode) {
        self.services.translate.set_language(language);
        let signal = self.services.language;
        self.app.app_mut().write(&signal, language);
    }

    pub(crate) fn store_account(&mut self, account: AccountConfig) {
        self.settings.store_account(self.app.app_mut(), account);
    }

    pub(crate) fn store_avatar(&mut self, wallet: &str, svg: &str) -> bool {
        self.settings
            .store_avatar_if_current(self.app.app_mut(), wallet, svg.to_owned())
    }

    pub(crate) fn clear_account(&mut self) {
        self.settings.clear_account(self.app.app_mut());
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
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
