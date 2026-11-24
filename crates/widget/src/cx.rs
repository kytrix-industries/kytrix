use std::rc::Rc;

use app_config::settings::Config;
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use tokio::runtime::Handle;
use ui_core::{
    a11y::A11yPatch,
    app::{App, AppHandle},
    error::EngineError,
    node::{
        id::NodeId,
        signal::{Signal, SignalRef, SignalScope, Transient},
        tree::NodeTree,
    },
    window::WindowId,
};
use ui_kit::{
    kit::{layer::Layer, overlay::AnimationRouter},
    theme::handle::ThemeHandle,
};

use super::{frame::FrameRouter, services::WidgetServices};

pub struct WidgetCx<'a> {
    app: &'a mut App,
    window: WindowId,
    services: &'a Rc<WidgetServices>,
    scope: SignalScope,
}

impl<'a> WidgetCx<'a> {
    pub fn new(
        app: &'a mut App,
        window: WindowId,
        services: &'a Rc<WidgetServices>,
        scope: SignalScope,
    ) -> Self {
        Self {
            app,
            window,
            services,
            scope,
        }
    }

    pub fn app(&self) -> &App {
        self.app
    }

    pub fn app_mut(&mut self) -> &mut App {
        self.app
    }

    pub const fn window(&self) -> WindowId {
        self.window
    }

    pub fn tree(&self) -> &NodeTree {
        self.app
            .window(self.window)
            .expect("a live widget's window exists")
            .tree()
    }

    pub fn tree_mut(&mut self) -> &mut NodeTree {
        self.app
            .window_mut(self.window)
            .expect("a live widget's window exists")
            .tree_mut()
    }

    pub fn services(&self) -> &WidgetServices {
        self.services
    }

    pub fn services_rc(&self) -> &Rc<WidgetServices> {
        self.services
    }

    pub fn translate(&self) -> &Translate {
        &self.services.translate
    }

    pub fn language(&self) -> Signal<LanguageCode> {
        self.services.language
    }

    pub fn theme(&self) -> &ThemeHandle {
        &self.services.theme
    }

    pub fn config_ref<R>(&self, read: impl FnOnce(&Config) -> R) -> R {
        self.app.read_with(&self.services.config, read)
    }

    pub fn account(&self) -> Option<SmolStr> {
        self.config_ref(crate::account::account_of)
    }

    pub fn data_time_zone(&self) -> time::UtcOffset {
        self.config_ref(crate::time_zone::data_time_zone)
    }

    pub fn window_mut(&mut self) -> &mut ui_core::window::Window {
        self.app
            .window_mut(self.window)
            .expect("a live widget's window exists")
    }

    pub fn set_canvas(
        &mut self,
        node: NodeId,
        canvas: ui_core::paint::canvas::Canvas,
    ) -> Result<bool, EngineError> {
        self.window_mut().set_canvas(node, canvas)
    }

    pub const fn scope(&self) -> SignalScope {
        self.scope
    }

    pub fn unique_key(&self, suffix: &str) -> SmolStr {
        format_smolstr!("{suffix}@{}", self.scope.index())
    }

    pub fn layer(&self) -> &Layer {
        &self.services.layer
    }

    pub fn frame_router(&self) -> &FrameRouter {
        &self.services.frame_router
    }

    pub fn animation(&self) -> &AnimationRouter {
        &self.services.animation_router
    }

    pub fn signal<T: Clone + 'static>(&mut self, initial: T) -> Signal<T> {
        self.app.signal_scoped(self.scope, initial)
    }

    pub fn transient<T: Clone + 'static>(&mut self, initial: T) -> Transient<T> {
        self.app.transient_scoped(self.scope, initial)
    }

    pub fn read<T: 'static>(&self, signal: &Signal<T>) -> SignalRef<T> {
        self.app.read(signal)
    }

    pub fn write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool {
        self.app.write(signal, value)
    }

    pub fn update<T: 'static>(&mut self, signal: &Signal<T>, update: impl FnOnce(&mut T)) -> bool {
        self.app.update(signal, update)
    }

    pub fn request_frame(&mut self) -> Result<(), EngineError> {
        self.app.request_repaint(self.window)
    }

    pub fn request_node_repaint(&mut self, node: NodeId) -> Result<bool, EngineError> {
        self.app.request_node_repaint(self.window, node)
    }

    pub fn set_a11y_patch(&mut self, node: NodeId, patch: A11yPatch) -> Result<(), EngineError> {
        self.app.set_a11y_patch(self.window, node, patch)
    }

    pub fn handle(&self) -> AppHandle {
        self.app.handle()
    }

    pub fn spawn(&mut self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.app.handle().spawn(future);
    }

    pub fn runtime(&self) -> &Handle {
        &self.services.runtime
    }
}

impl std::fmt::Debug for WidgetCx<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WidgetCx")
            .field("window", &self.window)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use app_config::settings::{AccountConfig, AccountWalletKind, Config};
    use smol_str::format_smolstr;
    use ui_core::{
        event::Action,
        math::{LogicalRect, px},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
        paint::canvas::Canvas,
    };
    use ui_kit::kit::button::Button;

    use crate::{cx::WidgetCx, test_support::WidgetTestApp};

    #[test]
    fn a_context_builds_nodes_in_the_widget_s_own_scope() {
        let mut harness = WidgetTestApp::new();
        let signal = harness.with_cx(|cx| cx.signal(7_u32));

        assert_eq!(*harness.app().read(&signal), 7);

        let node = harness.with_cx(|cx| {
            let parent = cx.tree().root();
            NodeBuilder::boxed()
                .build(cx.tree_mut(), parent)
                .expect("the root is alive")
        });
        let tree = harness.tree();
        assert_eq!(
            tree.get(node).expect("the node exists").kind(),
            &NodeKind::Box
        );
    }

    #[test]
    fn releasing_the_widget_scope_releases_the_signals_it_minted() {
        let mut harness = WidgetTestApp::new();
        let released = harness.with_cx(|cx| {
            let scope = cx.scope();
            cx.app_mut().release_scope(scope)
        });

        assert!(released > 0, "the harness's widget scope owns its signals");
    }

    #[test]
    fn the_settings_accessors_read_the_snapshot_without_cloning_it() {
        let mut harness = WidgetTestApp::new();
        let mut config = Config::default();
        config.account = Some(AccountConfig {
            user_id: String::from("11111111-1111-4111-8111-111111111111"),
            wallet_address: String::from("0x0000000000000000000000000000000000000001"),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");
        let config_signal = harness.services().config;
        assert!(harness.app_mut().write(&config_signal, config));

        harness.with_cx(|cx| {
            assert_eq!(
                cx.account().as_deref(),
                Some("11111111-1111-4111-8111-111111111111")
            );
            assert_eq!(cx.data_time_zone().whole_hours(), 8);
            assert_eq!(
                cx.config_ref(|config| crate::time_zone::data_time_zone(config).whole_hours()),
                8,
                "the same snapshot is visible through the borrowed read"
            );
        });
    }

    #[test]
    fn the_drawing_accessors_replace_a_node_canvas() {
        let mut harness = WidgetTestApp::new();
        let node = harness.with_cx(|cx| {
            let parent = cx.tree().root();
            NodeBuilder::canvas(Canvas::new(LogicalRect::new(
                px(0.0),
                px(0.0),
                px(8.0),
                px(8.0),
            )))
            .build(cx.tree_mut(), parent)
            .expect("the root is alive")
        });

        let changed = harness
            .with_cx(|cx| {
                cx.window_mut();
                cx.set_canvas(
                    node,
                    Canvas::new(LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0))),
                )
            })
            .expect("the node is alive");

        assert!(changed);
        assert!(matches!(
            harness.tree().get(node).expect("the node exists").kind(),
            NodeKind::Canvas(_)
        ));
    }

    #[test]
    fn the_context_hands_over_the_composition_root_s_services() {
        let mut harness = WidgetTestApp::new();

        let same_handle = harness.with_cx(|cx| std::ptr::eq(cx.services_rc(), cx.services_rc()));
        assert!(
            same_handle,
            "the context hands out one handle, not a fresh one per call"
        );

        let from_context = harness.with_cx(|cx| Rc::clone(cx.services_rc()));
        assert!(
            Rc::ptr_eq(&from_context, harness.services_rc()),
            "the handle is the composition root's instance, not a copy of it"
        );

        let layer = Rc::clone(&from_context.layer);
        assert!(
            std::ptr::eq(&*layer, harness.layer()),
            "a host built over that handle shares the application's layer stack"
        );
    }

    fn with_peer_cx<R>(
        harness: &mut WidgetTestApp,
        body: impl FnOnce(&mut WidgetCx<'_>) -> R,
    ) -> R {
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let scope = harness.app_mut().signal_scope();
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
        body(&mut cx)
    }

    fn build_keyed_button(cx: &mut WidgetCx<'_>, parent: NodeId, suffix: &str) -> (Button, Action) {
        let key = cx.unique_key(suffix);
        let theme = cx.theme().clone();
        let window = cx.window();
        let button = Button::new(cx.app_mut(), theme, key).on_click(|_| {});
        button
            .build(cx.app_mut(), window, parent)
            .expect("the parent is alive");
        let action = button
            .action_identity()
            .expect("a click callback registered the component's action");
        (button, action)
    }

    #[test]
    fn the_same_suffix_in_one_instance_keeps_one_key() {
        let mut harness = WidgetTestApp::new();

        let (within_one_call, again_in_one_call) =
            harness.with_cx(|cx| (cx.unique_key("sound"), cx.unique_key("sound")));
        assert_eq!(
            within_one_call, again_in_one_call,
            "two controls of one instance mint their own keys, not a fresh one per call"
        );

        let next_call = harness.with_cx(|cx| cx.unique_key("sound"));
        assert_eq!(
            within_one_call, next_call,
            "a control rebuilt on a later frame must reclaim the key it registered"
        );
    }

    #[test]
    fn the_key_carries_the_instance_scope_and_the_widget_local_suffix() {
        let mut harness = WidgetTestApp::new();

        let (key, scope) =
            harness.with_cx(|cx| (cx.unique_key("market_trade.header.sound"), cx.scope()));

        assert_eq!(
            key,
            format_smolstr!("market_trade.header.sound@{}", scope.index())
        );
        assert!(
            key.starts_with("market_trade.header.sound@"),
            "the suffix stays the prefix so a test can locate the control's debug id"
        );
    }

    #[test]
    fn two_instances_in_one_application_mint_different_keys() {
        let mut harness = WidgetTestApp::new();

        let first = harness.with_cx(|cx| (cx.scope(), cx.unique_key("sound")));
        let second = with_peer_cx(&mut harness, |cx| (cx.scope(), cx.unique_key("sound")));

        assert_ne!(
            first.0.index(),
            second.0.index(),
            "a widget mount mints its own signal scope"
        );
        assert_ne!(
            first.1, second.1,
            "two panes built the same suffix and must still get different keys"
        );
        assert_eq!(
            second.1,
            format_smolstr!("sound@{}", second.0.index()),
            "each key is stamped with its own instance's scope"
        );
    }

    #[test]
    fn different_suffixes_in_one_instance_stay_distinct() {
        let mut harness = WidgetTestApp::new();

        let (sound, confirm) =
            harness.with_cx(|cx| (cx.unique_key("header.sound"), cx.unique_key("confirm")));

        assert_ne!(sound, confirm);
        assert_eq!(sound, harness.with_cx(|cx| cx.unique_key("header.sound")));
    }

    #[test]
    fn releasing_one_pane_leaves_the_other_pane_s_action_handler() {
        let mut harness = WidgetTestApp::new();
        let root = harness.root();

        let (first_button, first_action) =
            harness.with_cx(|cx| build_keyed_button(cx, root, "market_trade.header.sound"));
        let (second_button, second_action) = with_peer_cx(&mut harness, |cx| {
            build_keyed_button(cx, root, "market_trade.header.sound")
        });

        assert_ne!(
            first_action, second_action,
            "each pane's control registered its own action identity"
        );
        assert!(harness.app().has_action_handler(&first_action));
        assert!(harness.app().has_action_handler(&second_action));

        first_button.release(harness.app_mut());
        assert!(
            !harness.app().has_action_handler(&first_action),
            "the closed pane's handler is unregistered"
        );
        assert!(
            harness.app().has_action_handler(&second_action),
            "releasing one pane removed another pane's handler — every instance uses the \
             same action key"
        );

        second_button.release(harness.app_mut());
        assert!(!harness.app().has_action_handler(&second_action));
    }
}
