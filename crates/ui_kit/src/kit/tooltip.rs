use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yRole,
    app::{App, TimerId},
    error::EngineError,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId, signal::SignalScope, tree::NodeTree},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    popup::{
        Alignment, Anchor, DismissPolicy, FocusPolicy, Placement, PlacementSpec, PopupA11y,
        PopupAnimation, PopupSpec, PopupTier, Side,
    },
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::popup_panel::{
        AnchorChange, AnchorWatch, PanelWidth, PopupPanel, PopupPanelOptions, PopupSurface,
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

#[derive(Clone, Debug, PartialEq)]
pub struct TooltipOptions {
    pub placement: PlacementSpec,
    pub show_delay: Duration,
    pub close_delay: Duration,
    pub animation: PopupAnimation,
    pub max_width: LogicalPx,
}

impl Default for TooltipOptions {
    fn default() -> Self {
        let mut placement = PlacementSpec {
            side: Side::Bottom,
            align: Alignment::Center,
            ..PlacementSpec::default()
        };
        placement.fallbacks.clear();
        placement
            .fallbacks
            .push(Placement::new(Side::Top, Alignment::Center));
        Self {
            placement,
            show_delay: Duration::from_millis(500),
            close_delay: Duration::from_millis(100),
            animation: PopupAnimation::Fade,
            max_width: px(320.0),
        }
    }
}

impl TooltipOptions {
    #[must_use]
    pub fn immediate(mut self) -> Self {
        self.show_delay = Duration::ZERO;
        self.close_delay = Duration::ZERO;
        self
    }

    #[must_use]
    pub fn show_delay(mut self, show_delay: Duration) -> Self {
        self.show_delay = show_delay;
        self
    }

    #[must_use]
    pub fn close_delay(mut self, close_delay: Duration) -> Self {
        self.close_delay = close_delay;
        self
    }

    #[must_use]
    pub fn placement(mut self, placement: PlacementSpec) -> Self {
        self.placement = placement;
        self
    }

    #[must_use]
    pub fn max_width(mut self, max_width: LogicalPx) -> Self {
        self.max_width = max_width;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    WaitingShow,
    Shown,
    WaitingClose,
}

struct TooltipInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: TooltipOptions,
    scope: SignalScope,
    panel: PopupPanel,
    timer: Cell<Option<TimerId>>,
    timer_ready: Cell<bool>,
    phase: Cell<Phase>,
    window: Cell<Option<WindowId>>,
    trigger: Cell<Option<NodeId>>,
    content: RefCell<Option<TooltipContent>>,
    shown: RefCell<Option<TooltipContent>>,
    pending_restart: Cell<bool>,
    suppressed: Cell<bool>,
    watch: RefCell<AnchorWatch>,
}

#[derive(Clone)]
pub struct Tooltip {
    inner: Rc<TooltipInner>,
}

impl Tooltip {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        options: TooltipOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        let panel = PopupPanel::new(
            app,
            theme.clone(),
            key.clone(),
            PopupPanelOptions::new(PopupSurface::tooltip(), PanelWidth::Auto),
        );
        Self {
            inner: Rc::new(TooltipInner {
                key,
                theme,
                options,
                scope,
                panel,
                timer: Cell::new(None),
                timer_ready: Cell::new(false),
                phase: Cell::new(Phase::Idle),
                window: Cell::new(None),
                trigger: Cell::new(None),
                content: RefCell::new(None),
                shown: RefCell::new(None),
                pending_restart: Cell::new(false),
                suppressed: Cell::new(false),
                watch: RefCell::new(AnchorWatch::default()),
            }),
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    pub fn attach(&self, app: &mut App, window: WindowId, trigger: NodeId) {
        if self.inner.trigger.get() != Some(trigger) || self.inner.window.get() != Some(window) {
            self.reset(app);
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
        matches!(self.inner.phase.get(), Phase::Shown | Phase::WaitingClose)
            && self.inner.panel.is_open(app)
    }

    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.panel.panel_node()
    }

    #[must_use]
    pub fn popup_node(&self) -> Option<NodeId> {
        self.inner.panel.popup_node()
    }

    pub fn sync(&self, app: &mut App) -> bool {
        let mut changed = self.inner.panel.sync(app);
        if matches!(self.inner.phase.get(), Phase::Shown | Phase::WaitingClose)
            && !self.inner.panel.is_open(app)
        {
            self.inner.suppressed.set(true);
            self.abandon(app);
            return true;
        }
        if matches!(self.inner.phase.get(), Phase::Shown | Phase::WaitingClose)
            && self.anchor_lost(app)
        {
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
        let has_content = self.inner.content.borrow().is_some();
        let content_changed = *self.inner.content.borrow() != *self.inner.shown.borrow();
        if self.inner.suppressed.get() {
            if !active {
                self.inner.suppressed.set(false);
            }
            return changed;
        }
        match self.inner.phase.get() {
            Phase::Idle => {
                if active && has_content {
                    if self.inner.pending_restart.get() {
                        if self.inner.panel.state(app).is_none() {
                            self.inner.pending_restart.set(false);
                            changed |= self.show(app);
                        }
                    } else if self.inner.panel.state(app).is_some()
                        || self.inner.options.show_delay.is_zero()
                    {
                        changed |= self.show(app);
                    } else {
                        self.start_timer(app, Phase::WaitingShow, self.inner.options.show_delay);
                        changed = true;
                    }
                } else if self.inner.panel.state(app).is_none() {
                    self.inner.pending_restart.set(false);
                }
            }
            Phase::WaitingShow => {
                if !active || !has_content {
                    self.reset(app);
                    changed = true;
                } else if self.inner.timer_ready.replace(false) {
                    changed |= self.show(app);
                }
            }
            Phase::Shown => {
                if !active || !has_content || content_changed {
                    self.inner
                        .pending_restart
                        .set(content_changed || !has_content);
                    self.begin_close(app);
                    changed = true;
                }
            }
            Phase::WaitingClose => {
                if active && has_content && !content_changed {
                    self.cancel_timer(app);
                    self.inner.phase.set(Phase::Shown);
                    changed = true;
                } else if self.inner.timer_ready.replace(false) {
                    self.close_panel(app);
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn release(&self, app: &mut App) {
        self.cancel_timer(app);
        self.inner.panel.release(app);
        let _ = app.release_scope(self.inner.scope);
    }

    fn live_trigger(&self, app: &App) -> Option<NodeId> {
        let window = self.inner.window.get()?;
        let trigger = self.inner.trigger.get()?;
        let tree = app.window(window)?.tree();
        tree.get(trigger).ok().map(|_| trigger)
    }

    fn anchor_lost(&self, app: &App) -> bool {
        let (Some(id), Some(window)) = (self.inner.panel.id(), self.inner.window.get()) else {
            return false;
        };
        self.inner.watch.borrow_mut().observe(app, id, window) == AnchorChange::Lost
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
        if [hovered, focused]
            .into_iter()
            .flatten()
            .any(|node| is_within(tree, node, trigger))
        {
            return true;
        }
        self.pointer_within_panel(app, window)
    }

    fn pointer_within_panel(&self, app: &App, window: WindowId) -> bool {
        let (Some(pointer), Some(id)) = (app.popup_pointer(window), self.inner.panel.id()) else {
            return false;
        };
        app.popup_rect(id)
            .is_some_and(|rect| rect.hit.contains(pointer) || rect.paint.contains(pointer))
    }

    fn start_timer(&self, app: &mut App, phase: Phase, delay: Duration) {
        self.cancel_timer(app);
        self.inner.phase.set(phase);
        if delay.is_zero() {
            self.inner.timer_ready.set(true);
            return;
        }
        let (Some(window), Some(trigger)) = (self.inner.window.get(), self.inner.trigger.get())
        else {
            return;
        };
        let ready = Rc::downgrade(&self.inner);
        let registered = app.after_scoped(window, trigger, delay, move |_app| {
            if let Some(inner) = ready.upgrade() {
                inner.timer.set(None);
                inner.timer_ready.set(true);
            }
        });
        match registered {
            Ok(id) => self.inner.timer.set(Some(id)),
            Err(_) => self.inner.timer_ready.set(true),
        }
    }

    fn cancel_timer(&self, app: &mut App) {
        self.inner.timer_ready.set(false);
        if let Some(id) = self.inner.timer.take() {
            let _ = app.cancel_timer(&id);
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
        let mut spec = PopupSpec::new(
            Anchor::Node { node: trigger },
            self.inner.options.placement.clone(),
        );
        spec.tier = PopupTier::Hint;
        spec.focus = FocusPolicy::None;
        spec.pointer_transparent = true;
        spec.dismiss = DismissPolicy::Hint { escape: true };
        spec.animation = self.inner.options.animation;
        spec.opener = Some(trigger);
        spec.a11y = PopupA11y {
            role: Some(A11yRole::Tooltip),
            label: Some(content.label()),
            described_by: Some(trigger),
            ..PopupA11y::default()
        };
        let built = content.clone();
        let opened = self
            .inner
            .panel
            .open(app, window, spec, move |app, window, body| {
                let Some(target) = app.window_mut(window) else {
                    return Err(EngineError::UnknownWindow { window });
                };
                build_content(target.tree_mut(), body, &key, &built, &theme, max_width)
            });
        match opened {
            Ok(_) => {
                self.inner.phase.set(Phase::Shown);
                self.inner.suppressed.set(false);
                self.inner.watch.borrow_mut().reset();
                *self.inner.shown.borrow_mut() = Some(content);
                true
            }
            Err(_) => {
                self.reset(app);
                false
            }
        }
    }

    fn abandon(&self, app: &mut App) {
        self.inner.phase.set(Phase::Idle);
        *self.inner.shown.borrow_mut() = None;
        self.inner.pending_restart.set(false);
        self.cancel_timer(app);
    }

    fn begin_close(&self, app: &mut App) {
        let close_delay = self.inner.options.close_delay;
        if close_delay.is_zero() {
            self.close_panel(app);
            return;
        }
        self.start_timer(app, Phase::WaitingClose, close_delay);
    }

    fn close_panel(&self, app: &mut App) {
        self.cancel_timer(app);
        self.inner.phase.set(Phase::Idle);
        let _ = self.inner.panel.close(app);
    }

    fn reset(&self, app: &mut App) {
        self.abandon(app);
        self.inner.suppressed.set(false);
        self.inner.watch.borrow_mut().reset();
        let _ = self.inner.panel.close_now(app);
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
    max_width: LogicalPx,
) -> Result<(), EngineError> {
    let limited = max_width.is_finite() && max_width > LogicalPx::ZERO;
    let mut row_style = StyleBuilder::new()
        .flex_row()
        .items_center()
        .gap(px(6.0))
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
        a11y::{A11yAction, A11yRole, A11yTreeSnapshot},
        error::EngineError,
        event::{Action, InputEvent, KeyCode, Modifiers},
        interaction::ScrollConfig,
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::style_builder::StyleBuilder,
        popup::{
            Alignment, DismissPolicy, FocusPolicy, InitialFocus, PlacementSpec, PopupId, PopupSpec,
            PopupTier, RestoreFocus, Side,
        },
        test_support::app::TestApp,
        window::{FrameOutcome, WindowId},
    };

    use super::{Tooltip, TooltipContent, TooltipOptions};
    use crate::theme::{appearance::Appearance, handle::ThemeHandle};

    const TRIGGER: LogicalRect = LogicalRect::from_origin_size(
        LogicalPoint::new(px(300.0), px(200.0)),
        LogicalSize::new(px(100.0), px(40.0)),
    );

    struct Harness {
        app: TestApp,
        window: WindowId,
        trigger: NodeId,
        tip: Tooltip,
    }

    impl Harness {
        fn new(options: TooltipOptions) -> Self {
            Self::with_content(options, TooltipContent::new("保存").shortcut("Ctrl+S"))
        }

        fn with_content(options: TooltipOptions, content: TooltipContent) -> Self {
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
            let tip = Tooltip::new(app.app_mut(), theme, "tip", options);
            tip.attach(app.app_mut(), window, trigger);
            tip.set_content(Some(content));
            let _ = app.drain().expect("首帧");
            Self {
                app,
                window,
                trigger,
                tip,
            }
        }

        fn hover(&mut self, at: LogicalPoint) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(at, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
        }

        fn sync(&mut self) -> bool {
            self.tip.sync(self.app.app_mut())
        }

        fn advance(&mut self, by: Duration) -> ui_core::test_support::record::FrameRecord {
            self.app.advance(by);
            self.app.drain().expect("帧").clone()
        }

        fn pump(&mut self, by: Duration) -> ui_core::test_support::record::FrameRecord {
            self.app.advance(by);
            let _ = self.sync();
            self.app.drain().expect("帧").clone()
        }

        fn settle(&mut self) {
            for _ in 0..3 {
                self.advance(Duration::ZERO);
                let _ = self.sync();
            }
            let _ = self.app.drain().expect("帧");
        }

        fn trigger_center(&self) -> LogicalPoint {
            let bounds = self.app.probe().bounds(self.trigger).expect("触发器布局");
            LogicalPoint::new(
                bounds.origin.x + bounds.size.width * 0.5,
                bounds.origin.y + bounds.size.height * 0.5,
            )
        }

        fn panel_rect(&self) -> LogicalRect {
            let panel = self.tip.panel_node().expect("面板");
            self.app.probe().bounds(panel).expect("面板布局")
        }

        fn pump_until_unloaded(&mut self, frames: usize) -> bool {
            for _ in 0..frames {
                self.pump(Duration::from_millis(16));
                if self.tip.popup_node().is_none() {
                    return true;
                }
            }
            self.tip.popup_node().is_none()
        }

        fn texts_of(&self, root: NodeId) -> BTreeSet<String> {
            let mut texts = BTreeSet::new();
            collect_texts(
                self.app.app().window(self.window).expect("窗口").tree(),
                root,
                &mut texts,
            );
            texts
        }

        fn modal_open(&self) -> bool {
            self.app.app().modal_scope(self.window).is_some()
        }

        fn stack(&self) -> Vec<(PopupId, PopupTier, u64)> {
            self.app
                .app()
                .popup_stack(self.window)
                .expect("栈")
                .entries
                .iter()
                .map(|entry| (entry.id, entry.tier, entry.z))
                .collect()
        }

        fn open_modal(
            &mut self,
            build: impl FnOnce(&mut NodeTree, NodeId) -> Result<(), EngineError>,
        ) -> PopupId {
            let mut spec = PopupSpec::new(
                ui_core::popup::Anchor::WindowRect(LogicalRect::from_origin_size(
                    LogicalPoint::new(px(40.0), px(40.0)),
                    LogicalSize::new(px(1.0), px(1.0)),
                )),
                PlacementSpec::default(),
            );
            spec.tier = PopupTier::Modal;
            spec.focus = FocusPolicy::Trap {
                restore: RestoreFocus::Opener,
                initial: InitialFocus::Container,
            };
            spec.dismiss = DismissPolicy::Explicit {
                outside: false,
                escape: true,
            };
            spec.animation = ui_core::popup::PopupAnimation::None;
            let app = self.app.app_mut();
            let window = self.window;
            let id = app
                .open_popup(
                    window,
                    spec,
                    move |app, window, node| {
                        let Some(target) = app.window_mut(window) else {
                            return Err(EngineError::UnknownWindow { window });
                        };
                        build(target.tree_mut(), node)
                    },
                    None,
                )
                .expect("打开模态");
            let _ = self.app.drain().expect("帧");
            id
        }

        fn a11y(&mut self) -> A11yTreeSnapshot {
            self.app.enable_a11y_capture();
            let _ = self.app.drain().expect("a11y 帧");
            self.app.a11y().expect("a11y 快照").clone()
        }
    }

    fn collect_texts(tree: &NodeTree, node: NodeId, out: &mut BTreeSet<String>) {
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

    #[test]
    fn a_tooltip_opens_only_after_its_delay() {
        let mut harness = Harness::new(TooltipOptions {
            show_delay: Duration::from_millis(500),
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
            "延迟不得用动画驱动"
        );

        harness.app.advance(Duration::from_millis(250));
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "定时器等待期不得按帧空转");
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "延迟中途不得显示"
        );

        harness.app.advance(Duration::from_millis(300));
        let _ = harness.app.drain().expect("到期帧");
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "定时器回调只置位，打开动作由下一轮钩子做"
        );
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()), "延迟结束后显示");
        harness.settle();
        assert!(harness.tip.popup_node().is_some());
    }

    #[test]
    fn leaving_the_trigger_hides_the_tooltip_and_the_close_animation_unloads_it() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()), "零延迟立即显示");
        harness.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = harness.sync();

        harness.hover(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "离开触发器即收起"
        );
        assert!(
            harness.tip.popup_node().is_some(),
            "淡出期间节点保留（关闭动画由引擎驱动）"
        );
        assert!(
            harness.pump_until_unloaded(20),
            "淡出必须在 20 轮内走完并卸载节点"
        );
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(harness.stack().is_empty(), "关闭后浮层栈清空");
    }

    #[test]
    fn the_close_delay_keeps_the_tooltip_during_a_quick_pointer_jump() {
        let mut harness = Harness::new(TooltipOptions {
            show_delay: Duration::ZERO,
            close_delay: Duration::from_millis(100),
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        harness.hover(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "关闭延迟窗口内不得立即收起（离开触发器的宽限）"
        );
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "宽限内移回：保持显示"
        );

        harness.hover(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = harness.sync();
        harness.advance(Duration::from_millis(120));
        let _ = harness.app.drain().expect("到期帧");
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()), "宽限到期后收起");
    }

    #[test]
    fn focusing_the_trigger_opens_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
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
    fn the_tooltip_is_placed_below_the_trigger_by_the_engine_placement() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.settle();
        let panel = harness.panel_rect();
        assert!(
            (panel.origin.y - (TRIGGER.max().y + 4.0)).value().abs() < 0.5,
            "Bottom 定位：面板顶边 = 触发器底边 + gap，实际 {panel:?}"
        );
        assert!(
            (panel.origin.x + panel.size.width * 0.5
                - (TRIGGER.origin.x + TRIGGER.size.width * 0.5))
                .value()
                .abs()
                < 0.5,
            "Bottom 居中：面板中心应贴触发器中心，实际 {panel:?}"
        );
    }

    #[test]
    fn an_explicit_top_placement_puts_the_tooltip_above_the_trigger() {
        let placement = PlacementSpec {
            side: Side::Top,
            align: Alignment::Center,
            ..PlacementSpec::default()
        };
        let gap = placement.gap;
        let mut harness = Harness::new(TooltipOptions::default().immediate().placement(placement));
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.settle();
        let panel = harness.panel_rect();
        assert!(
            (panel.max().y - (TRIGGER.origin.y - gap)).value().abs() < 0.5,
            "TOP 定位：面板底边 = 触发器顶边 - gap，实际 {panel:?}"
        );
        assert!(
            (panel.origin.x + panel.size.width * 0.5
                - (TRIGGER.origin.x + TRIGGER.size.width * 0.5))
                .value()
                .abs()
                < 0.5,
            "TOP 居中：面板中心应贴触发器中心，实际 {panel:?}"
        );
    }

    #[test]
    fn the_tooltip_renders_its_text_and_shortcut() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.settle();
        let panel = harness.tip.panel_node().expect("面板");
        let texts = harness.texts_of(panel);
        assert!(texts.contains("保存"), "提示文本渲染，实际 {texts:?}");
        assert!(texts.contains("Ctrl+S"), "快捷键渲染，实际 {texts:?}");
    }

    #[test]
    fn a_tooltip_wraps_its_text_at_the_configured_max_width() {
        const LONG: &str = "迁移期的提示内容在窄面板里必须按配置的宽度上限换行，而不是把浮层撑成一条横跨整个窗口的细长条。";

        fn panel_size(options: TooltipOptions) -> (f32, f32) {
            let mut harness = Harness::with_content(options.immediate(), TooltipContent::new(LONG));
            harness.hover(harness.trigger_center());
            let _ = harness.sync();
            harness.settle();
            let bounds = harness.panel_rect();
            (f32::from(bounds.size.width), f32::from(bounds.size.height))
        }

        assert_eq!(
            TooltipOptions::default().max_width,
            px(320.0),
            "默认 320 px 内容上限"
        );
        let (default_width, default_height) = panel_size(TooltipOptions::default());
        assert!(
            (default_width - (320.0 + 8.0)).abs() <= 1.0,
            "默认上限 320：面板总宽 = 320 + 2 × 面板内边距，实际 {default_width}"
        );

        let (narrow_width, narrow_height) =
            panel_size(TooltipOptions::default().max_width(px(120.0)));
        assert!(
            (narrow_width - (120.0 + 8.0)).abs() <= 1.0,
            "显式上限 120 生效，实际面板宽 {narrow_width}"
        );
        assert!(
            narrow_height > default_height,
            "上限更窄 ⇒ 换行更多 ⇒ 面板更高（{narrow_height} vs {default_height}）"
        );

        let (wide_width, wide_height) =
            panel_size(TooltipOptions::default().max_width(px(f32::INFINITY)));
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
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        let _ = harness.app.drain().expect("帧");

        let wrapper = harness.tip.popup_node().expect("浮层内容节点");
        let panel = harness.tip.panel_node().expect("面板");
        let snapshot = harness.a11y();
        let record = snapshot.node_for(wrapper).expect("浮层 a11y");
        assert_eq!(record.role, A11yRole::Tooltip, "提示角色");
        assert_eq!(
            record.label_text(),
            Some("保存（Ctrl+S）"),
            "可访问名含文本与快捷键"
        );
        assert!(
            record.actions.contains(&A11yAction::HideTooltip),
            "Tooltip 只暴露 AT 的隐藏动作（dismiss 语义），实际 {:?}",
            record.actions
        );
        assert!(
            !record.actions.iter().any(|action| matches!(
                action,
                A11yAction::Click | A11yAction::Focus | A11yAction::Dismiss
            )),
            "提示不得暴露点击 / 聚焦 / dismiss 动作，实际 {:?}",
            record.actions
        );
        let panel_record = snapshot.node_for(panel).expect("面板 a11y");
        assert_eq!(
            panel_record.role,
            A11yRole::GenericContainer,
            "面板不得重复声明 Tooltip"
        );
    }

    #[test]
    fn a_tooltip_does_not_take_focus() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        let other = {
            let root = harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .root();
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("focusable")
                .layout_override(LogicalRect::from_origin_size(
                    LogicalPoint::new(px(10.0), px(10.0)),
                    LogicalSize::new(px(20.0), px(20.0)),
                ))
                .tab_stop(true)
                .on_click(Action::Custom(format_smolstr!("focusable.click")))
                .build(tree, root)
                .expect("可聚焦节点")
        };
        assert!(harness.app.app_mut().set_focus(harness.window, Some(other)));
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.settle();
        assert!(harness.tip.is_visible(harness.app.app()));
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(other),
            "提示打开不得夺走焦点（FocusPolicy::None）"
        );
    }

    #[test]
    fn moving_the_pointer_onto_the_tooltip_keeps_it_shown() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.settle();
        assert!(harness.tip.is_visible(harness.app.app()));
        let panel = harness.panel_rect();
        let center = LogicalPoint::new(
            panel.origin.x + panel.size.width * 0.5,
            panel.origin.y + panel.size.height * 0.5,
        );
        assert!(!TRIGGER.contains(center), "前置：指针不在触发器内");
        harness.hover(center);
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "hover 区域必须包含提示自身 rect（WCAG 1.4.13）"
        );
    }

    #[test]
    fn a_tooltip_does_not_intercept_pointer_input() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        let target_rect = LogicalRect::from_origin_size(
            LogicalPoint::new(px(240.0), px(240.0)),
            LogicalSize::new(px(260.0), px(120.0)),
        );
        let clicks = Rc::new(Cell::new(0_u32));
        let action = Action::Custom(format_smolstr!("target.click"));
        let counter = Rc::clone(&clicks);
        harness
            .app
            .app_mut()
            .on_action(action.clone(), move |_ctx| {
                counter.set(counter.get() + 1);
            });
        let target = {
            let root = harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .root();
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("target")
                .layout_override(target_rect)
                .on_click(action)
                .build(tree, root)
                .expect("下层控件")
        };

        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.settle();
        assert!(harness.tip.is_visible(harness.app.app()));
        let panel = harness.panel_rect();
        let point = LogicalPoint::new(
            panel.origin.x + panel.size.width * 0.5,
            panel.origin.y + panel.size.height * 0.5,
        );
        assert!(
            target_rect.contains(point),
            "前置：提示覆盖在下层控件之上，panel={panel:?} target={target_rect:?}"
        );
        let hit = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .hit_test(point)
            .map(|hit| hit.node);
        assert_eq!(
            hit,
            Some(target),
            "pointer_transparent 提示不得参与指针命中"
        );

        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            ui_core::event::PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = harness.app.inject(InputEvent::pointer_up(
            point,
            ui_core::event::PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = harness.app.drain().expect("帧");
        assert_eq!(clicks.get(), 1, "提示上的点击必须穿透到下层控件");
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "穿透点击不得关闭提示"
        );
    }

    #[test]
    fn withdrawing_the_content_hides_an_open_tooltip() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        assert!(harness.tip.set_content(None), "内容变化必须被感知");
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "撤回内容 = 收起已显示的提示"
        );
    }

    #[test]
    fn re_hovering_during_the_fade_out_shows_the_tooltip_again() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(200));
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        harness.hover(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(harness.tip.popup_node().is_some(), "淡出期间节点还活着");

        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "淡出中移回指针：提示重新显示"
        );
        assert!(harness.tip.popup_node().is_some(), "重开后面板节点存在");
    }

    #[test]
    fn a_missing_trigger_resets_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
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
        assert!(
            harness.pump_until_unloaded(20),
            "触发器消失后锚点丢失，提示关闭动画结束后卸载"
        );
    }

    #[test]
    fn losing_focus_hides_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(harness.trigger));
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()), "焦点显示");

        harness.app.app_mut().set_focus(harness.window, None);
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "失焦必须关闭提示"
        );
    }

    #[test]
    fn escape_closes_only_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
        let _ = harness.app.drain().expect("帧");
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()), "Esc 关闭提示");
        assert!(harness.pump_until_unloaded(20), "Esc 关闭动画结束后卸载");
        assert!(harness.stack().is_empty(), "Esc 只关自身且清空提示栈");
    }

    #[test]
    fn changing_the_content_while_shown_rebuilds_the_panel() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(200));
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));
        let old_panel = harness.tip.panel_node().expect("旧面板");
        assert!(
            harness.texts_of(old_panel).contains("保存"),
            "前置：屏上是旧文本"
        );

        assert!(harness.tip.set_content(Some(TooltipContent::new("新建"))));
        let _ = harness.sync();
        assert!(
            harness.tip.popup_node().is_some(),
            "旧面板先淡出（节点暂时保留）"
        );
        harness.pump(Duration::from_millis(16));
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "内容变化的淡出窗口内不得复活旧面板"
        );

        for _ in 0..20 {
            harness.pump(Duration::from_millis(16));
            if harness.tip.is_visible(harness.app.app()) {
                break;
            }
        }
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "内容变化后重新显示"
        );
        let panel = harness.tip.panel_node().expect("新面板");
        let texts = harness.texts_of(panel);
        assert!(
            texts.contains("新建"),
            "新文本必须重建到屏上，实际 {texts:?}"
        );
        assert!(!texts.contains("保存"), "旧文本不得残留，实际 {texts:?}");
    }

    #[test]
    fn restoring_content_during_the_fade_rebuilds_with_the_new_text() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(200));
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        assert!(harness.tip.set_content(None));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(harness.tip.popup_node().is_some(), "前置：进入淡出窗口");
        assert!(harness.tip.set_content(Some(TooltipContent::new("新建"))));

        harness.pump(Duration::from_millis(16));
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "撤回→恢复不得在淡出窗口内撤销关闭"
        );

        for _ in 0..20 {
            harness.pump(Duration::from_millis(16));
            if harness.tip.is_visible(harness.app.app()) {
                break;
            }
        }
        assert!(harness.tip.is_visible(harness.app.app()));
        let panel = harness.tip.panel_node().expect("新面板");
        let texts = harness.texts_of(panel);
        assert!(
            texts.contains("新建"),
            "恢复的新文本必须重建，实际 {texts:?}"
        );
        assert!(!texts.contains("保存"), "旧文本不得残留，实际 {texts:?}");
    }

    #[test]
    fn re_entering_during_the_content_change_fade_still_rebuilds_the_new_text() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(200));
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        assert!(harness.tip.set_content(Some(TooltipContent::new("新建"))));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));

        harness.hover(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = harness.sync();
        assert!(harness.tip.popup_node().is_some(), "离开后仍在淡出窗口内");
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "内容变化的淡出窗口内重新进入不得复活旧面板"
        );

        for _ in 0..20 {
            harness.pump(Duration::from_millis(16));
            if harness.tip.is_visible(harness.app.app()) {
                break;
            }
        }
        assert!(harness.tip.is_visible(harness.app.app()));
        let panel = harness.tip.panel_node().expect("新面板");
        let texts = harness.texts_of(panel);
        assert!(
            texts.contains("新建"),
            "重开的面板必须是新文本，实际 {texts:?}"
        );
        assert!(!texts.contains("保存"), "旧文本不得复活，实际 {texts:?}");
    }

    #[test]
    fn leaving_during_the_delay_prevents_the_stale_deadline_from_showing() {
        let mut harness = Harness::new(TooltipOptions::default());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.hover(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));

        harness.advance(Duration::from_millis(700));
        let _ = harness.app.drain().expect("帧");
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "离开后旧的到期点不得生效"
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
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        let _dialog = harness.open_modal(|tree, panel| {
            NodeBuilder::boxed()
                .id("dialog-box")
                .style(
                    StyleBuilder::new()
                        .width(px(120.0))
                        .height(px(40.0))
                        .build(),
                )
                .build(tree, panel)?;
            Ok(())
        });
        assert!(harness.modal_open(), "前置：模态作用域存在");

        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "背景触发器不得在模态之上打开提示"
        );
        assert!(harness.tip.popup_node().is_none());
        assert!(
            harness.app.app().hovered(harness.window).is_none(),
            "模态作用域外的 hover 必须读作 None"
        );
        assert!(harness.modal_open(), "提示被抑制后模态作用域保持不变");
    }

    #[test]
    fn a_modal_hides_an_open_tooltip_and_blocks_new_ones_until_it_closes() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));

        let dialog = harness.open_modal(|_, _| Ok(()));
        let _ = harness.app.drain().expect("帧");
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "Modal 打开时 Hint 必须隐藏/关闭"
        );
        for _ in 0..20 {
            harness.pump(Duration::from_millis(16));
            if harness.tip.popup_node().is_none() {
                break;
            }
        }
        assert!(harness.tip.popup_node().is_none(), "提示节点被引擎回收");
        assert!(
            harness.modal_open(),
            "前置：模态仍打开（提示关闭不关闭模态）"
        );

        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "Modal 打开期间任何 Hint 都不得打开（引擎 HintBlockedByModal）"
        );
        assert!(harness.tip.popup_node().is_none());

        assert!(harness.app.app_mut().close_popup(dialog).is_ok());
        let _ = harness.app.drain().expect("帧");
        assert!(!harness.modal_open());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "Modal 关闭后由 hover 重新触发提示"
        );
    }

    #[test]
    fn opening_and_closing_a_tooltip_never_forces_a_full_repaint() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        for _ in 0..3 {
            let record = harness.pump(Duration::ZERO);
            assert!(!record.damage_full, "打开提示不得全量重绘");
        }
        assert!(harness.tip.is_visible(harness.app.app()));

        harness.hover(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = harness.sync();
        for _ in 0..10 {
            let record = harness.pump(Duration::from_millis(16));
            assert!(!record.damage_full, "关闭提示不得全量重绘");
        }
        assert!(harness.tip.popup_node().is_none(), "关闭动画在 10 帧内走完");
    }

    #[test]
    fn releasing_a_visible_tooltip_unloads_it() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));
        harness.tip.release(harness.app.app_mut());
        assert!(harness.tip.popup_node().is_none(), "release 立即卸载面板");
        assert!(harness.stack().is_empty(), "release 清空浮层栈");
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
    }

    #[test]
    fn immediate_disables_both_delays() {
        assert_eq!(
            TooltipOptions::default().show_delay,
            Duration::from_millis(500),
            "默认显示延迟 500 ms"
        );
        assert_eq!(
            TooltipOptions::default().close_delay,
            Duration::from_millis(100),
            "默认关闭延迟 100 ms"
        );
        let immediate = TooltipOptions::default().immediate();
        assert_eq!(
            immediate.show_delay,
            Duration::ZERO,
            "immediate() 关闭显示延迟"
        );
        assert_eq!(
            immediate.close_delay,
            Duration::ZERO,
            "immediate() 关闭关闭延迟"
        );
    }

    #[test]
    fn a_tooltip_does_not_close_an_open_menu_and_stays_above_it() {
        let mut harness = Harness::new(TooltipOptions::default().immediate());
        let mut menu_spec = PopupSpec::new(
            ui_core::popup::Anchor::WindowRect(LogicalRect::from_origin_size(
                LogicalPoint::new(px(20.0), px(20.0)),
                LogicalSize::new(px(1.0), px(1.0)),
            )),
            PlacementSpec::default(),
        );
        menu_spec.tier = PopupTier::Auto;
        menu_spec.focus = FocusPolicy::None;
        menu_spec.animation = ui_core::popup::PopupAnimation::None;
        let app = harness.app.app_mut();
        let window = harness.window;
        let menu = app
            .open_popup(window, menu_spec, |_, _, _| Ok(()), None)
            .expect("打开菜单");
        let _ = harness.app.drain().expect("菜单帧");

        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(
            harness.tip.is_visible(harness.app.app()),
            "提示可以叠加在菜单上"
        );
        let stack = harness.stack();
        let menu_entry = stack
            .iter()
            .find(|(id, _, _)| *id == menu)
            .copied()
            .expect("菜单仍在栈里（Hint 不得关闭 Auto 根）");
        let tip_node = harness.tip.popup_node().expect("提示节点");
        let entries = harness.app.app().popup_stack(harness.window).expect("栈");
        let tip_entry = entries
            .entries
            .iter()
            .find(|entry| entry.node == tip_node)
            .copied()
            .expect("提示条目");
        assert_eq!(menu_entry.1, PopupTier::Auto);
        assert!(tip_entry.z > menu_entry.2, "提示 z 高于已打开的菜单");
    }

    #[test]
    fn scrolling_the_anchor_repositions_the_tooltip_and_hides_it_when_it_leaves_the_view() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let root = app.app().window(window).expect("窗口").tree().root();
        let page = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .id("tooltip-page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(px(800.0))
                        .height(px(300.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("页面")
        };
        let trigger = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .id("tooltip-scroll-trigger")
                .layout_override(LogicalRect::new(px(20.0), px(20.0), px(100.0), px(40.0)))
                .build(tree, page)
                .expect("触发器")
        };
        let scroll = app
            .app_mut()
            .make_scroll(window, page, ScrollConfig::default())
            .expect("滚动容器");
        app.app_mut()
            .set_scroll_content(&scroll, LogicalSize::new(px(800.0), px(2400.0)))
            .expect("滚动内容");
        let tip = Tooltip::new(
            app.app_mut(),
            theme,
            "scroll-tip",
            TooltipOptions::default().immediate(),
        );
        tip.attach(app.app_mut(), window, trigger);
        tip.set_content(Some(TooltipContent::new("保存")));
        let _ = app.drain().expect("首帧");

        let _ = app.inject(InputEvent::pointer_move(
            LogicalPoint::new(px(70.0), px(40.0)),
            Modifiers::empty(),
        ));
        let _ = app.drain().expect("悬停帧");
        let _ = tip.sync(app.app_mut());
        assert!(tip.is_visible(app.app()), "悬停后提示必须显示");
        let node = tip.popup_node().expect("提示节点");
        let id = app
            .app()
            .popup_stack(window)
            .expect("栈")
            .entries
            .iter()
            .find(|entry| entry.node == node)
            .expect("提示条目")
            .id;
        let before = app.app().popup_rect(id).expect("提示 rect").hit;

        app.app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(10.0)))
            .expect("滚动");
        let _ = app.drain().expect("滚动帧");
        let _ = tip.sync(app.app_mut());
        assert!(tip.is_visible(app.app()), "锚点仍可见时提示保持显示");
        let after = app.app().popup_rect(id).expect("提示 rect").hit;
        assert_ne!(after.origin, before.origin, "滚动后提示必须重新定位");

        app.app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(200.0)))
            .expect("滚出视口");
        let _ = app.drain().expect("滚出帧");
        let _ = tip.sync(app.app_mut());
        assert!(!tip.is_visible(app.app()), "锚点滚出视口必须隐藏提示");
        assert!(
            tip.popup_node().is_none(),
            "隐藏必须立即卸载提示（不得进入关闭延迟）"
        );
    }
}
