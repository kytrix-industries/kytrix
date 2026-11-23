use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::{App, TimerId},
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::SignalScope, tree::NodeTree},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        layer::{Band, Layer},
        overlay::{
            Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayA11y, OverlayOptions,
            OverlaySurface, Placement,
        },
    },
    theme::{components::ComponentTokens, handle::ThemeHandle, role::Role},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TooltipContent {
    pub text: SmolStr,
    pub shortcut: Option<SmolStr>,
}

impl TooltipContent {
    #[must_use]
    pub fn new(text: impl Into<SmolStr>) -> Self {
        Self {
            text: text.into(),
            shortcut: None,
        }
    }

    #[must_use]
    pub fn shortcut(mut self, shortcut: impl Into<SmolStr>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    #[must_use]
    pub fn label(&self) -> SmolStr {
        match &self.shortcut {
            Some(shortcut) => format_smolstr!("{text}（{shortcut}）", text = self.text),
            None => self.text.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TooltipOptions {
    pub placement: Placement,
    pub gap: f32,
    pub delay: Duration,
    pub fade: bool,
    pub max_width: f32,
}

impl Default for TooltipOptions {
    fn default() -> Self {
        Self {
            placement: Placement::BOTTOM,
            gap: 4.0,
            delay: Duration::from_millis(500),
            fade: true,
            max_width: 320.0,
        }
    }
}

impl TooltipOptions {
    #[must_use]
    pub fn immediate(mut self) -> Self {
        self.delay = Duration::ZERO;
        self
    }

    #[must_use]
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    #[must_use]
    pub fn max_width(mut self, max_width: f32) -> Self {
        self.max_width = max_width;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    Waiting,
    Shown,
}

struct TooltipInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: TooltipOptions,
    scope: SignalScope,
    overlay: Overlay,
    delay_timer: Cell<Option<TimerId>>,
    delay_ready: Cell<bool>,
    phase: Cell<Phase>,
    window: Cell<Option<WindowId>>,
    trigger: Cell<Option<NodeId>>,
    content: RefCell<Option<TooltipContent>>,
    shown: RefCell<Option<TooltipContent>>,
    pending_restart: Cell<bool>,
}

#[derive(Clone)]
pub struct Tooltip {
    inner: Rc<TooltipInner>,
}

impl Tooltip {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
        options: TooltipOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        let overlay = Overlay::new(
            app,
            theme.clone(),
            layer,
            router,
            format_smolstr!("{key}:overlay"),
            OverlayOptions {
                placement: options.placement,
                gap: options.gap,
                margin: 4.0,
                flip: true,
                dismiss: DismissPolicy::Explicit,
                modal: false,
                restore_focus: false,
                band: Band::Tooltip,
                slot: crate::kit::layer::LayerSlot::Exclusive,
                surface: OverlaySurface::tooltip(),
                scrim: None,
                a11y: OverlayA11y::default(),
                fade: options.fade,
            },
        );
        Self {
            inner: Rc::new(TooltipInner {
                scope,
                key,
                theme,
                options,
                overlay,
                delay_timer: Cell::new(None),
                delay_ready: Cell::new(false),
                phase: Cell::new(Phase::Idle),
                window: Cell::new(None),
                trigger: Cell::new(None),
                content: RefCell::new(None),
                shown: RefCell::new(None),
                pending_restart: Cell::new(false),
            }),
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    pub fn attach(&self, app: &mut App, window: WindowId, trigger: NodeId) {
        if self.inner.trigger.get() != Some(trigger) || self.inner.window.get() != Some(window) {
            self.hide(app);
        }
        self.inner.window.set(Some(window));
        self.inner.trigger.set(Some(trigger));
    }

    pub fn set_content(&self, content: Option<TooltipContent>) -> bool {
        let mut current = self.inner.content.borrow_mut();
        if *current == content {
            return false;
        }
        *current = content;
        true
    }

    #[must_use]
    pub fn content(&self) -> Option<TooltipContent> {
        self.inner.content.borrow().clone()
    }

    #[must_use]
    pub fn is_visible(&self, app: &App) -> bool {
        self.inner.phase.get() == Phase::Shown && self.inner.overlay.is_open(app)
    }

    #[must_use]
    pub fn overlay(&self) -> &Overlay {
        &self.inner.overlay
    }

    pub fn sync(&self, app: &mut App) -> bool {
        let mut changed = self.inner.overlay.sync(app);
        if self.inner.phase.get() == Phase::Shown && !self.inner.overlay.is_open(app) {
            self.reset(app);
            return true;
        }
        let Some(trigger) = self.live_trigger(app) else {
            if self.inner.phase.get() != Phase::Idle {
                self.reset(app);
                return true;
            }
            return changed;
        };
        let active = self.is_active(app, trigger);
        match self.inner.phase.get() {
            Phase::Idle => {
                let has_content = self.inner.content.borrow().is_some();
                if active && has_content {
                    if self.inner.overlay.is_open(app) {
                        if !self.inner.pending_restart.get() {
                            changed |= self.show(app);
                        }
                    } else if self.inner.pending_restart.replace(false) {
                        changed |= self.show(app);
                    } else {
                        self.start_delay(app);
                        changed = true;
                    }
                } else if !self.inner.overlay.is_open(app) {
                    self.inner.pending_restart.set(false);
                }
            }
            Phase::Waiting => {
                if !active || self.inner.content.borrow().is_none() {
                    self.reset(app);
                    changed = true;
                } else if self.inner.delay_ready.replace(false) {
                    changed |= self.show(app);
                }
            }
            Phase::Shown => {
                let withdrawn = self.inner.content.borrow().is_none();
                let content_changed = *self.inner.content.borrow() != *self.inner.shown.borrow();
                if !active || withdrawn {
                    self.hide(app);
                    self.inner.pending_restart.set(withdrawn || content_changed);
                    changed = true;
                } else if content_changed {
                    self.hide(app);
                    self.inner.pending_restart.set(true);
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn release(&self, app: &mut App) {
        self.cancel_delay(app);
        self.inner.overlay.release(app);
        let _ = app.release_scope(self.inner.scope);
    }

    fn live_trigger(&self, app: &App) -> Option<NodeId> {
        let window = self.inner.window.get()?;
        let trigger = self.inner.trigger.get()?;
        let tree = app.window(window)?.tree();
        tree.get(trigger).ok().map(|_| trigger)
    }

    fn is_active(&self, app: &App, trigger: NodeId) -> bool {
        let Some(window) = self.inner.window.get() else {
            return false;
        };
        let Some(tree) = app.window(window).map(|target| target.tree()) else {
            return false;
        };
        let hovered = app.hovered(window);
        let focused = app.focused(window);
        [hovered, focused]
            .into_iter()
            .flatten()
            .any(|node| is_within(tree, node, trigger))
    }

    fn start_delay(&self, app: &mut App) {
        self.cancel_delay(app);
        let (Some(window), Some(trigger)) = (self.inner.window.get(), self.inner.trigger.get())
        else {
            return;
        };
        if self.inner.options.delay.is_zero() {
            self.inner.phase.set(Phase::Waiting);
            let _ = self.show(app);
            return;
        }
        self.inner.phase.set(Phase::Waiting);
        let ready = Rc::downgrade(&self.inner);
        let registered = app.after_scoped(window, trigger, self.inner.options.delay, move |_app| {
            if let Some(inner) = ready.upgrade() {
                inner.delay_timer.set(None);
                inner.delay_ready.set(true);
            }
        });
        match registered {
            Ok(id) => self.inner.delay_timer.set(Some(id)),
            Err(_) => self.reset(app),
        }
    }

    fn show(&self, app: &mut App) -> bool {
        let (Some(window), Some(trigger), Some(content)) = (
            self.inner.window.get(),
            self.inner.trigger.get(),
            self.inner.content.borrow().clone(),
        ) else {
            return false;
        };
        let theme = self.inner.theme.clone();
        let key = self.inner.key.clone();
        let max_width = self.inner.options.max_width;
        self.inner
            .overlay
            .set_a11y(OverlayA11y::tooltip(content.label()));
        let built = content.clone();
        let opened =
            self.inner
                .overlay
                .open(app, window, Anchor::node(trigger), move |tree, panel| {
                    build_content(tree, panel, &key, &built, &theme, max_width)
                });
        match opened {
            Ok(opened) => {
                self.inner.phase.set(Phase::Shown);
                *self.inner.shown.borrow_mut() = Some(content);
                opened
            }
            Err(_) => {
                self.reset(app);
                false
            }
        }
    }

    fn hide(&self, app: &mut App) {
        if self.inner.phase.get() == Phase::Shown {
            let _ = self.inner.overlay.close(app);
        }
        self.inner.phase.set(Phase::Idle);
        *self.inner.shown.borrow_mut() = None;
        self.cancel_delay(app);
    }

    fn reset(&self, app: &mut App) {
        self.inner.phase.set(Phase::Idle);
        *self.inner.shown.borrow_mut() = None;
        self.inner.pending_restart.set(false);
        self.cancel_delay(app);
        let _ = self.inner.overlay.close_now(app);
    }

    fn cancel_delay(&self, app: &mut App) {
        self.inner.delay_ready.set(false);
        if let Some(id) = self.inner.delay_timer.take() {
            let _ = app.cancel_timer(&id);
        }
    }
}

fn is_within(tree: &NodeTree, node: NodeId, ancestor: NodeId) -> bool {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    false
}

fn build_content(
    tree: &mut NodeTree,
    panel: NodeId,
    key: &str,
    content: &TooltipContent,
    theme: &ThemeHandle,
    max_width: f32,
) -> Result<(), EngineError> {
    let limited = max_width.is_finite() && max_width > 0.0;
    let mut row_style = StyleBuilder::new()
        .flex_row()
        .items_center()
        .gap(6.0)
        .min_width_0();
    if limited {
        row_style = row_style.max_width(max_width);
    }
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:tip"))
        .style(row_style.build())
        .build(tree, panel)?;
    let tokens = ComponentTokens::standard();
    let size = tokens.small.font_size;
    let mut text_style = TextStyle::new(size);
    text_style.color = theme_role(theme, Role::FgOnInverse);
    let theme_text = theme.clone();
    NodeBuilder::text(content.text.clone(), text_style)
        .style(StyleBuilder::new().min_width_0().build())
        .bind_style(move |cx| StylePatch {
            foreground: Some(theme_text.components(cx).tooltip.foreground),
            ..StylePatch::default()
        })
        .build(tree, row)?;
    if let Some(shortcut) = &content.shortcut {
        let mut shortcut_style = TextStyle::new(size - 1.0);
        shortcut_style.weight = 500;
        shortcut_style.color = theme_role(theme, Role::FgOnInverse).with_opacity(0.7);
        let theme_shortcut = theme.clone();
        NodeBuilder::text(shortcut.clone(), shortcut_style)
            .style(StyleBuilder::new().flex_none().build())
            .bind_style(move |cx| StylePatch {
                foreground: Some(
                    theme_shortcut
                        .components(cx)
                        .tooltip
                        .foreground
                        .with_opacity(0.7),
                ),
                ..StylePatch::default()
            })
            .build(tree, row)?;
    }
    Ok(())
}

fn theme_role(theme: &ThemeHandle, role: Role) -> ui_core::paint::color::Color {
    theme
        .theme_for(crate::theme::appearance::Appearance::Light)
        .role(role)
}

#[cfg(test)]
mod tests {

    use std::{cell::Cell, collections::BTreeSet, rc::Rc, time::Duration};

    use smol_str::format_smolstr;
    use ui_core::{
        a11y::A11yRole,
        error::EngineError,
        event::{Action, InputEvent, KeyCode, Modifiers},
        math::{Point, Rect},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::style_builder::StyleBuilder,
        test_support::app::TestApp,
        window::{FrameOutcome, WindowId},
    };

    use super::{Tooltip, TooltipContent, TooltipOptions};
    use crate::{
        kit::{
            layer::{Band, Layer},
            overlay::{
                Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayA11y, OverlayOptions,
                OverlaySurface, Placement,
            },
        },
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    const TRIGGER: Rect = Rect::new(300.0, 200.0, 100.0, 40.0);

    struct Harness {
        app: TestApp,
        window: WindowId,
        trigger: NodeId,
        tip: Tooltip,
        layer: Layer,
        router: AnimationRouter,
        theme: ThemeHandle,
    }

    impl Harness {
        fn new(options: TooltipOptions) -> Self {
            Self::with_layer(options, Layer::new())
        }

        fn with_layer(options: TooltipOptions, layer: Layer) -> Self {
            Self::with_content(
                options,
                layer,
                TooltipContent::new("保存").shortcut("Ctrl+S"),
            )
        }

        fn with_content(options: TooltipOptions, layer: Layer, content: TooltipContent) -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let root = app.app().window(window).expect("窗口").tree().root();
            let trigger = {
                let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
                NodeBuilder::boxed()
                    .id("trigger")
                    .layout_override(TRIGGER)
                    .on_click(Action::Custom(format_smolstr!("trigger.click")))
                    .build(tree, root)
                    .expect("触发器")
            };
            let router = AnimationRouter::new();
            let tip = Tooltip::new(
                app.app_mut(),
                theme.clone(),
                layer.clone(),
                router.clone(),
                "tip",
                options,
            );
            tip.attach(app.app_mut(), window, trigger);
            tip.set_content(Some(content));
            let _ = app.drain().expect("首帧");
            Self {
                app,
                window,
                trigger,
                tip,
                layer,
                router,
                theme,
            }
        }

        fn hover(&mut self, at: Point) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(at, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
        }

        fn sync(&mut self) -> bool {
            self.tip.sync(self.app.app_mut())
        }

        fn advance(&mut self, by: Duration) {
            self.app.advance(by);
            let _ = self.app.drain().expect("帧");
        }

        fn pump(&mut self, by: Duration) {
            self.app.advance(by);
            let _ = self.sync();
            let _ = self.app.drain().expect("帧");
        }

        fn settle(&mut self) {
            for _ in 0..3 {
                self.advance(Duration::ZERO);
                let _ = self.sync();
            }
            let _ = self.app.drain().expect("帧");
        }

        fn open_modal(
            &mut self,
            content: impl FnOnce(&mut NodeTree, NodeId) -> Result<(), EngineError>,
        ) -> Overlay {
            let dialog = Overlay::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                "dialog",
                OverlayOptions {
                    modal: true,
                    dismiss: DismissPolicy::Explicit,
                    surface: OverlaySurface::dialog(),
                    a11y: OverlayA11y::dialog("对话框"),
                    fade: false,
                    ..OverlayOptions::default()
                },
            );
            assert!(
                dialog
                    .open(
                        self.app.app_mut(),
                        self.window,
                        Anchor::point(Point::new(50.0, 50.0)),
                        content
                    )
                    .expect("打开模态对话框")
            );
            let _ = self.app.drain().expect("帧");
            let _ = dialog.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
            dialog
        }

        fn trigger_center(&self) -> Point {
            let bounds = self.app.probe().bounds(self.trigger).expect("触发器布局");
            Point::new(
                bounds.origin.x + bounds.size.width * 0.5,
                bounds.origin.y + bounds.size.height * 0.5,
            )
        }
    }

    #[test]
    fn a_tooltip_opens_only_after_its_delay() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::from_millis(500),
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        assert!(harness.sync(), "悬停后进入延迟（登记定时器）");
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "延迟未走完不得显示"
        );
        assert!(
            !harness.app.app().animations_running(harness.window),
            "延迟不得用动画驱动（E10 定时器；旧写法会在延迟期每帧 tick）"
        );

        harness.app.advance(Duration::from_millis(250));
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(
            outcome,
            FrameOutcome::Idle,
            "定时器等待期不得按帧空转（旧动画延迟会每帧 tick，这里会红）"
        );
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "延迟中途不得显示"
        );

        harness.app.advance(Duration::from_millis(300));
        let _ = harness.app.drain().expect("到期帧");
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "定时器回调只置位，打开动作由下一轮钩子做（状态机单一入口）"
        );
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()), "延迟结束后显示");
        harness.settle();
        assert!(harness.tip.overlay().overlay_node().is_some());
    }

    #[test]
    fn leaving_the_trigger_hides_the_tooltip_and_the_fade_completion_unloads_it() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()), "零延迟立即显示");
        harness.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = harness.sync();
        assert_eq!(
            harness.tip.overlay().progress(harness.app.app()),
            1.0,
            "淡入完成"
        );

        harness.hover(Point::new(10.0, 10.0));
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "离开触发器即收起"
        );
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "淡出期间节点保留"
        );
        let mut unloaded_in_the_completion_frame = false;
        for _ in 0..20 {
            harness.pump(Duration::from_millis(16));
            if !harness.app.app().animations_running(harness.window) {
                assert!(
                    harness.tip.overlay().overlay_node().is_none(),
                    "动画完成的那一轮 drain 之后节点必须已卸载（完成回调同帧收尾；\
                     等下一轮 sync 的旧形态会在这里变红）"
                );
                unloaded_in_the_completion_frame = true;
                break;
            }
        }
        assert!(
            unloaded_in_the_completion_frame,
            "淡出必须在 20 轮内走完并触发完成回调"
        );
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(!harness.tip.overlay().is_open(harness.app.app()));
    }

    #[test]
    fn focusing_the_trigger_opens_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(harness.trigger));
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "焦点落在触发器上时也要显示（键盘用户）"
        );
    }

    #[test]
    fn the_tooltip_is_placed_above_the_trigger_by_the_overlay_placement() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            placement: Placement::TOP,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.settle();
        let panel = harness
            .app
            .probe()
            .bounds(harness.tip.overlay().overlay_node().expect("浮层节点"))
            .expect("面板布局");
        assert!(
            (panel.max().y - (TRIGGER.origin.y - 4.0)).abs() < 0.5,
            "Top 定位：面板底边 = 触发器顶边 − gap，实际 {panel:?}"
        );
        assert!(
            (panel.origin.x + panel.size.width * 0.5
                - (TRIGGER.origin.x + TRIGGER.size.width * 0.5))
                .abs()
                < 0.5,
            "Top 居中：面板中心应贴触发器中心，实际 {panel:?}"
        );
    }

    #[test]
    fn the_tooltip_renders_its_text_and_shortcut() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.settle();
        let panel = harness.tip.overlay().panel_node().expect("面板");
        let mut texts = BTreeSet::new();
        collect_texts(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree(),
            panel,
            &mut texts,
        );
        assert!(texts.contains("保存"), "提示文本渲染，实际 {texts:?}");
        assert!(texts.contains("Ctrl+S"), "快捷键渲染，实际 {texts:?}");
    }

    #[test]
    fn a_tooltip_wraps_its_text_at_the_configured_max_width() {
        const LONG: &str = "迁移期的提示内容在窄面板里必须按配置的宽度上限换行，而不是把浮层撑成一条横跨整个窗口的细长条。";

        fn panel_size(options: TooltipOptions) -> (f32, f32) {
            let mut harness =
                Harness::with_content(options.immediate(), Layer::new(), TooltipContent::new(LONG));
            harness.hover(harness.trigger_center());
            let _ = harness.sync();
            harness.settle();
            let panel = harness.tip.overlay().panel_node().expect("面板");
            let bounds = harness.app.probe().bounds(panel).expect("面板布局");
            (bounds.size.width, bounds.size.height)
        }

        assert_eq!(
            TooltipOptions::default().max_width,
            320.0,
            "默认恢复旧实现的 320 px 内容上限（W3a F6）"
        );
        let (default_width, default_height) = panel_size(TooltipOptions::default());
        assert!(
            (default_width - (320.0 + 8.0)).abs() <= 1.0,
            "默认上限 320：面板总宽 = 320 + 2 × 面板内边距，实际 {default_width}"
        );

        let (narrow_width, narrow_height) = panel_size(TooltipOptions::default().max_width(120.0));
        assert!(
            (narrow_width - (120.0 + 8.0)).abs() <= 1.0,
            "显式上限 120 生效，实际面板宽 {narrow_width}"
        );
        assert!(
            narrow_height > default_height,
            "上限更窄 ⇒ 换行更多 ⇒ 面板更高（{narrow_height} vs {default_height}）"
        );

        let (wide_width, wide_height) =
            panel_size(TooltipOptions::default().max_width(f32::INFINITY));
        assert!(
            wide_width > default_width + 100.0,
            "f32::INFINITY = 不限制：长句单行撑开，实际 {wide_width}"
        );
        assert!(
            wide_height < default_height,
            "单行比换行矮（{wide_height} vs {default_height}）"
        );
    }

    #[test]
    fn a_tooltip_is_not_interactive_and_declares_the_tooltip_role() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.app.enable_a11y_capture();
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        let _ = harness.app.drain().expect("帧");

        let wrapper = harness.tip.overlay().overlay_node().expect("浮层包装节点");
        let panel = harness.tip.overlay().panel_node().expect("面板");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(wrapper).expect("包装节点 a11y");
        assert_eq!(record.role, A11yRole::Tooltip, "提示角色");
        assert_eq!(
            record.label_text(),
            Some("保存（Ctrl+S）"),
            "可访问名含文本与快捷键"
        );
        assert!(
            record.actions.is_empty(),
            "提示没有任何 a11y 动作（不可交互）"
        );
        let panel_record = snapshot.node_for(panel).expect("面板 a11y");
        assert_eq!(
            panel_record.role,
            A11yRole::GenericContainer,
            "面板不得重复声明 Tooltip（E12-R1：AT 不读两层同名角色）"
        );
    }

    #[test]
    fn withdrawing_the_content_hides_an_open_tooltip() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        assert!(harness.tip.set_content(None), "内容变化必须被感知");
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "撤回内容 = 收起已显示的提示（旧实现的 hover 语义）"
        );
    }

    #[test]
    fn re_hovering_during_the_fade_out_shows_the_tooltip_again() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(200));
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        harness.hover(Point::new(10.0, 10.0));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "淡出期间节点还活着"
        );

        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "淡出中移回指针：提示立即淡回（撤销关闭）"
        );
    }

    #[test]
    fn a_missing_trigger_resets_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));
        {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            tree.remove(harness.trigger).expect("移除触发器");
        }
        let _ = harness.app.drain().expect("帧");
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(harness.tip.overlay().overlay_node().is_none());
    }

    #[test]
    fn losing_focus_hides_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(harness.trigger));
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "焦点在触发器上时显示"
        );

        harness.app.app_mut().set_focus(harness.window, None);
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "失焦必须关闭提示"
        );
    }

    #[test]
    fn escape_closes_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
        let _ = harness.app.drain().expect("帧");
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()), "Esc 关闭提示");
        assert!(harness.tip.overlay().overlay_node().is_none());
    }

    #[test]
    fn changing_the_content_while_shown_rebuilds_the_panel() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = harness.sync();
        assert_eq!(
            harness.tip.overlay().progress(harness.app.app()),
            1.0,
            "前置：淡入已完成（否则内容变化走 close_now 立即卸载路径，覆盖不到 pending_restart）"
        );
        let old_panel = harness.tip.overlay().panel_node().expect("旧面板");
        assert!(
            texts_of(&harness, old_panel).contains("保存"),
            "前置：屏上是旧文本"
        );

        assert!(
            harness.tip.set_content(Some(TooltipContent::new("新建"))),
            "内容变化必须被感知"
        );
        let _ = harness.sync();
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "旧面板先淡出（节点暂时保留）"
        );

        harness.pump(Duration::from_millis(16));
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "内容变化的淡出窗口内不得复活旧面板（M4 变异：这里会变可见）"
        );
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：仍在淡出窗口内，旧面板还没卸载"
        );

        for _ in 0..20 {
            harness.pump(Duration::from_millis(16));
            if harness.tip.is_visible(harness.app.app()) {
                break;
            }
        }
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "内容变化后按新内容重新显示"
        );
        let panel = harness.tip.overlay().panel_node().expect("新面板");
        let texts = texts_of(&harness, panel);
        assert!(
            texts.contains("新建"),
            "新文本必须重建到屏上，实际 {texts:?}"
        );
        assert!(!texts.contains("保存"), "旧文本不得残留，实际 {texts:?}");
    }

    #[test]
    fn restoring_content_during_the_fade_rebuilds_with_the_new_text() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));
        assert_eq!(harness.tip.overlay().progress(harness.app.app()), 1.0);

        assert!(harness.tip.set_content(None));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：撤回后进入淡出窗口"
        );
        assert!(harness.tip.set_content(Some(TooltipContent::new("新建"))));

        harness.pump(Duration::from_millis(16));
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "撤回→恢复不得在淡出窗口内撤销关闭（N1 变异：这里会复活旧面板）"
        );
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：仍在淡出窗口内"
        );

        for _ in 0..20 {
            harness.pump(Duration::from_millis(16));
            if harness.tip.is_visible(harness.app.app()) {
                break;
            }
        }
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "恢复内容后按新内容重新显示"
        );
        let panel = harness.tip.overlay().panel_node().expect("新面板");
        let texts = texts_of(&harness, panel);
        assert!(
            texts.contains("新建"),
            "恢复的新文本必须重建到屏上，实际 {texts:?}"
        );
        assert!(!texts.contains("保存"), "旧文本不得残留，实际 {texts:?}");
    }

    #[test]
    fn re_entering_during_the_content_change_fade_still_rebuilds_the_new_text() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "前置：提示已显示"
        );
        harness.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = harness.sync();
        assert_eq!(harness.tip.overlay().progress(harness.app.app()), 1.0);
        let old_panel = harness.tip.overlay().panel_node().expect("旧面板");
        assert!(
            texts_of(&harness, old_panel).contains("保存"),
            "前置：屏上是旧文本"
        );

        assert!(harness.tip.set_content(Some(TooltipContent::new("新建"))));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：仍在淡出窗口内"
        );

        harness.hover(Point::new(10.0, 10.0));
        let _ = harness.sync();
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：离开后仍在淡出窗口内"
        );
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "内容变化的淡出窗口内重新进入不得复活旧面板"
        );
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：仍在淡出窗口内，旧面板尚未卸载"
        );

        for _ in 0..20 {
            harness.pump(Duration::from_millis(16));
            if harness.tip.is_visible(harness.app.app()) {
                break;
            }
        }
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "旧面板关闭后必须按新内容重新显示"
        );
        let panel = harness.tip.overlay().panel_node().expect("新面板");
        let texts = texts_of(&harness, panel);
        assert!(
            texts.contains("新建"),
            "重开的面板必须是新文本，实际 {texts:?}"
        );
        assert!(!texts.contains("保存"), "旧文本不得复活，实际 {texts:?}");
    }

    #[test]
    fn leaving_during_the_delay_prevents_the_stale_deadline_from_showing() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::from_millis(500),
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.hover(Point::new(10.0, 10.0));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));

        harness.advance(Duration::from_millis(700));
        let _ = harness.app.drain().expect("帧");
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "离开后旧的到期点不得生效（否则提示会在没有悬停时弹出）"
        );

        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(400));
        let _ = harness.app.drain().expect("帧");
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "重新进入后必须重新计时（400 ms 时不得显示）"
        );
        harness.advance(Duration::from_millis(150));
        let _ = harness.app.drain().expect("到期帧");
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "重新计满延迟后显示"
        );
    }

    #[test]
    fn a_modal_scope_suppresses_a_background_tooltip() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        let _dialog = harness.open_modal(|tree, panel| {
            NodeBuilder::boxed()
                .id("dialog-box")
                .style(StyleBuilder::new().width(120.0).height(40.0).build())
                .build(tree, panel)?;
            Ok(())
        });
        assert!(
            harness.app.app().modal_scope(harness.window).is_some(),
            "前置：模态作用域存在（E12 公开查询面）"
        );

        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "背景触发器不得在模态之上打开提示（撤销 E12 的 hover 过滤时这里会红）"
        );
        assert!(harness.tip.overlay().overlay_node().is_none());
        assert!(
            harness.app.app().hovered(harness.window).is_none(),
            "模态作用域外的 hover 必须读作 None（E12/R07；引擎过滤是唯一口径）"
        );
        assert!(
            harness.app.app().modal_scope(harness.window).is_some(),
            "提示被抑制后模态作用域保持不变"
        );
    }

    #[test]
    fn a_tooltip_triggered_inside_the_modal_scope_still_works_and_stays_above_it() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        let inner_slot = Rc::new(Cell::new(None));
        let slot = Rc::clone(&inner_slot);
        let dialog = harness.open_modal(move |tree, panel| {
            let node = NodeBuilder::boxed()
                .id("inner-trigger")
                .layout_override(Rect::new(10.0, 10.0, 100.0, 40.0))
                .build(tree, panel)?;
            slot.set(Some(node));
            Ok(())
        });
        let inner = inner_slot.get().expect("模态内的触发器");
        harness
            .tip
            .attach(harness.app.app_mut(), harness.window, inner);
        let bounds = harness.app.probe().bounds(inner).expect("触发器布局");
        let center = Point::new(
            bounds.origin.x + bounds.size.width * 0.5,
            bounds.origin.y + bounds.size.height * 0.5,
        );
        harness.hover(center);
        assert_eq!(
            harness.app.app().hovered(harness.window),
            Some(inner),
            "模态作用域内的悬停必须可见"
        );
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "模态内部的触发器照常显示提示"
        );
        let tip_node = harness.tip.overlay().overlay_node().expect("提示节点");
        let dialog_node = dialog.overlay_node().expect("对话框节点");
        let order: Vec<NodeId> = harness
            .app
            .app()
            .overlays(harness.window)
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        let tip_at = order
            .iter()
            .position(|node| *node == tip_node)
            .expect("提示");
        let dialog_at = order
            .iter()
            .position(|node| *node == dialog_node)
            .expect("对话框");
        assert!(
            tip_at > dialog_at,
            "提示必须画在对话框之上（Band::Tooltip 最高），实际 {order:?}"
        );
    }

    #[test]
    fn immediate_disables_the_delay() {
        assert_eq!(
            TooltipOptions::default().delay,
            Duration::from_millis(500),
            "默认延迟 500 ms"
        );
        assert_eq!(
            TooltipOptions::default().immediate().delay,
            Duration::ZERO,
            "immediate() 关闭延迟"
        );
    }

    #[test]
    fn a_tooltip_is_above_a_context_menu_in_the_shared_layer() {
        let mut harness = Harness::with_layer(
            TooltipOptions {
                delay: Duration::ZERO,
                ..TooltipOptions::default()
            },
            Layer::new(),
        );
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));
        let tip_node = harness.tip.overlay().overlay_node().expect("提示节点");

        let menu = Overlay::new(
            harness.app.app_mut(),
            harness.theme.clone(),
            harness.layer.clone(),
            harness.router.clone(),
            "menu",
            OverlayOptions {
                band: Band::ContextMenu,
                dismiss: DismissPolicy::Explicit,
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(
            menu.open(
                harness.app.app_mut(),
                harness.window,
                Anchor::point(Point::new(20.0, 20.0)),
                |_, _| Ok(())
            )
            .expect("打开菜单")
        );
        let menu_node = menu.overlay_node().expect("菜单节点");

        let order: Vec<NodeId> = harness
            .app
            .app()
            .overlays(harness.window)
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        let menu_at = order
            .iter()
            .position(|node| *node == menu_node)
            .expect("菜单");
        let tip_at = order
            .iter()
            .position(|node| *node == tip_node)
            .expect("提示");
        assert!(
            tip_at > menu_at,
            "提示必须在 context menu 之上（共享层带 Tooltip > ContextMenu），实际 {order:?}"
        );
    }

    fn texts_of(harness: &Harness, root: NodeId) -> BTreeSet<String> {
        let mut texts = BTreeSet::new();
        collect_texts(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree(),
            root,
            &mut texts,
        );
        texts
    }

    fn collect_texts(
        tree: &ui_core::node::tree::NodeTree,
        node: NodeId,
        out: &mut BTreeSet<String>,
    ) {
        let Ok(target) = tree.get(node) else {
            return;
        };
        if let ui_core::node::tree::NodeKind::Text(content) = target.kind() {
            out.insert(content.text.to_string());
        }
        for child in target.children() {
            collect_texts(tree, *child, out);
        }
    }
}
