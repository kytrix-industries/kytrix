//! 按钮（旧 `crate::button` 的引擎化重建，P2-4a）。
//!
//! 旧 `Button` 是 `Interactive` 的薄壳：默认 `medium` 尺寸、`Filled` 变体，`RenderOnce` 里只补
//! `rounded_sm` 与 `Role::Button`。新实现保留**同样的分工**：状态机、点击 / 键盘激活、a11y 状态与
//! 焦点都在 [`Interactive`]（本批的通用件），本模块只加按钮特有的四件事：
//!
//! 1. **变体**：`primary` / `secondary` / `outline` / `ghost` 直接映射主题交互配方；`danger` 用
//!    主题状态角色（`StatusDangerBase` / `FgOnBrand`）表达的 [`RolePalette`]（旧实现没有 danger
//!    变体，这是本批新增，登记见 spec-delta §3）。
//! 2. **尺寸阶梯**：`small` / `medium` / `large` 写进布局样式（`kit::size`），字号进 `TextStyle`。
//! 3. **图标 + 文字**：图标是 E7 图片节点，颜色由 `bind_style` 的 `foreground` 动态调制（与文字同
//!    一条通道）；布局是 `gap(size.gap())` 的行。
//! 4. **加载态**：`loading` 与禁用同配方（不可交互 + `busy`），子节点不重建 —— 旧按钮没有加载
//!    态，这是本批新增的最小语义；旋转指示器由调用方在旁边挂 [`crate::kit::spinner::Spinner`]。
//!
//! # 与旧 API 的差异
//!
//! - 旧按钮从 `Interactive` 继承 `Styled` / `ParentElement` / `InteractiveElement`；新 API 是
//!   挂载期 builder：`build(app, window, parent)` + `sync(app)` + `dispatch(app, event)`；
//! - 旧调用方用 `.child(...)` 任意挂内容；新按钮只画「图标 + 文字」（content 在挂载期一次决定），
//!   任意内容用 [`Interactive`] 直接建；
//! - `on_click` 的回调在 `build` 时注册成 Action handler；纯引擎风格可以只给 `.action(..)` 自己
//!   注册 handler；
//! - 拼接（旧 `Join::button`）只做到**外角圆角**（`joined_edges` 的角半径写进 `PaintSpec`）；
//!   引擎的绘制边框只有统一宽度，共享边无法只画一条线（登记见 spec-delta §3）。

use std::{cell::RefCell, rc::Rc};

use smol_str::SmolStr;
use taffy::style::{Dimension, JustifyContent};
use ui_core::{
    app::App,
    error::EngineError,
    event::{Action, Event},
    interaction::ActionCtx,
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        icon::IconKind,
        interactive::{Interactive, RolePalette, StateRoles, mount_dynamic_icon},
        join::joined_edges,
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{components::InteractiveVariant, role::Role},
};

/// 圆角（旧 `rounded_sm` 的 4 px）。
const CORNER_RADIUS: f32 = 4.0;

/// 按钮变体。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonVariant {
    /// 饱和品牌块（旧默认；主操作）。
    #[default]
    Primary,
    /// 控件色块（次操作）。
    Secondary,
    /// 透明块 + 边缘。
    Outline,
    /// 只有文字与 hover 洗色。
    Ghost,
    /// 破坏性操作（本批新增：主题状态角色配方）。
    Danger,
}

/// 按钮（`Rc` 浅拷贝；状态在 [`Interactive`] 里）。
#[derive(Clone)]
pub struct Button {
    interactive: Interactive,
    variant: ButtonVariant,
    size: UnifiedSize,
    icon: Option<IconKind>,
    label: Option<SmolStr>,
    a11y_label: Option<SmolStr>,
    disabled: bool,
    loading: bool,
    selected: bool,
    action: Option<Action>,
    callback: Option<Rc<dyn Fn(&mut ActionCtx<'_>)>>,
    tooltip: Option<SmolStr>,
    /// 拼接位置 `(index, count)`（`None` = 不拼接）。
    joined: Option<(usize, usize)>,
    geometry: Rc<RefCell<Option<NodeId>>>,
}

impl Button {
    /// 建按钮（默认 `Primary` / `Medium`）。`key` 是动作身份与调试 id 前缀。
    pub fn new(
        app: &mut App,
        theme: crate::theme::handle::ThemeHandle,
        key: impl Into<SmolStr>,
    ) -> Self {
        Self {
            interactive: Interactive::new(app, theme, key),
            variant: ButtonVariant::Primary,
            size: UnifiedSize::Medium,
            icon: None,
            label: None,
            a11y_label: None,
            disabled: false,
            loading: false,
            selected: false,
            action: None,
            callback: None,
            tooltip: None,
            joined: None,
            geometry: Rc::new(RefCell::new(None)),
        }
    }

    /// 主按钮（默认，显式拼写）。
    #[must_use]
    pub fn primary(mut self) -> Self {
        self.variant = ButtonVariant::Primary;
        self
    }

    /// 次按钮（`Tonal`）。
    #[must_use]
    pub fn secondary(mut self) -> Self {
        self.variant = ButtonVariant::Secondary;
        self
    }

    /// 描边按钮。
    #[must_use]
    pub fn outline(mut self) -> Self {
        self.variant = ButtonVariant::Outline;
        self
    }

    /// 静默按钮（工具栏 / 菜单）。
    #[must_use]
    pub fn ghost(mut self) -> Self {
        self.variant = ButtonVariant::Ghost;
        self
    }

    /// 破坏性按钮。
    #[must_use]
    pub fn danger(mut self) -> Self {
        self.variant = ButtonVariant::Danger;
        self
    }

    /// 任意变体。
    #[must_use]
    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    /// 图标（画在文字左侧；颜色随状态动态调制）。
    #[must_use]
    pub fn icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    /// 文字（同时作为 a11y 可访问名）。
    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 只给 a11y 的可访问名（icon-only 按钮必需；`label` 仍画可见文字，两者都给时用这个当名字）。
    ///
    /// 旧 `Interactive` 只能把可见文字当名字，图标按钮因此在 AT 树里是「无名按钮」；本方法是
    /// 最小补齐（P2-4d1 审查 D5）。文案由调用方给（i18n 在调用侧解析）。
    #[must_use]
    pub fn a11y_label(mut self, label: impl Into<SmolStr>) -> Self {
        self.a11y_label = Some(label.into());
        self
    }

    /// 禁用。
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// 加载态（不可交互 + a11y `busy`）。
    #[must_use]
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    /// 选中态（画成 `Active`）。
    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// 显式动作身份。
    #[must_use]
    pub fn action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    /// 点击 / Enter / Space 激活回调。
    #[must_use]
    pub fn on_click(mut self, callback: impl Fn(&mut ActionCtx<'_>) + 'static) -> Self {
        self.callback = Some(Rc::new(callback));
        self
    }

    /// tooltip 文本挂点（P2-4b 读取）。
    #[must_use]
    pub fn tooltip_text(mut self, text: impl Into<SmolStr>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// 拼接位置（`index` / `count`；只改外角圆角）。
    #[must_use]
    pub fn joined(mut self, index: usize, count: usize) -> Self {
        self.joined = Some((index, count));
        self
    }

    /// 按钮节点（`build` 之后可用）。
    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        *self.geometry.borrow()
    }

    /// 内部状态机（组合控件 / 测试用）。
    #[must_use]
    pub fn interactive(&self) -> &Interactive {
        &self.interactive
    }

    /// 动作身份（显式给的，或由 `on_click` 自动生成的 `"<key>.click"`）。
    #[must_use]
    pub fn action_identity(&self) -> Option<Action> {
        self.action.clone().or_else(|| {
            self.callback
                .as_ref()
                .map(|_| Action::Custom(smol_str::format_smolstr!("{}.click", self.key())))
        })
    }

    /// 按钮的动作 key 前缀（调试 id 同源）。
    #[must_use]
    pub fn key(&self) -> &str {
        self.interactive.key()
    }

    /// 建按钮节点与子节点（图标 → 文字），并注册点击回调。
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode` / 图标资源解码与建树错误。
    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let mut interactive = self.interactive.clone().with_unified_size(self.size);
        if let Some(label) = self.a11y_label.as_ref().or(self.label.as_ref()) {
            interactive = interactive.label(label.clone());
        }
        if let Some(action) = &self.action {
            interactive = interactive.action(action.clone());
        }
        if let Some(tooltip) = &self.tooltip {
            interactive = interactive.tooltip_text(tooltip.clone());
        }
        interactive = match self.variant {
            ButtonVariant::Primary => interactive.variant(InteractiveVariant::Filled),
            ButtonVariant::Secondary => interactive.variant(InteractiveVariant::Tonal),
            ButtonVariant::Outline => interactive.variant(InteractiveVariant::Outline),
            ButtonVariant::Ghost => interactive.variant(InteractiveVariant::Ghost),
            ButtonVariant::Danger => interactive.role_palette(danger_palette()),
        };
        interactive = interactive
            .disabled(self.disabled)
            .loading(self.loading)
            .selected(self.selected);
        if let Some((index, count)) = self.joined {
            let radii = joined_edges(CORNER_RADIUS, 1.0, index, count).corner_radius;
            interactive = interactive.corner_radii(radii);
        }
        if let Some(callback) = &self.callback {
            let callback = callback.clone();
            interactive = interactive.on_click(move |ctx| callback(ctx));
        }
        let label = self.label.clone();
        let icon = self.icon;
        let size = self.size;
        let for_icon = interactive.clone();
        let for_text = interactive.clone();
        let watcher = interactive.clone();
        let node = interactive.build(app, window, parent, move |app, window, parent| {
            if let Some(kind) = icon {
                let icon_node =
                    mount_dynamic_icon(app, window, parent, kind, size.icon_size(), move |cx| {
                        StylePatch {
                            foreground: Some(for_icon.foreground_color(cx)),
                            ..StylePatch::default()
                        }
                    })?;
                watcher.watch(icon_node);
            }
            if let Some(text) = label {
                let tree = app
                    .window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut();
                let label_node = NodeBuilder::text(text, TextStyle::new(size.text_size()))
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(for_text.foreground_color(cx)),
                        ..StylePatch::default()
                    })
                    .build(tree, parent)?;
                watcher.watch(label_node);
            }
            Ok(())
        })?;
        *self.geometry.borrow_mut() = Some(node);
        Ok(node)
    }

    /// 帧驱动同步（悬停 / 焦点 / 状态过渡 / 按下态）。
    ///
    /// # Errors
    /// 同 [`Interactive::sync`]。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        self.interactive.sync(app)
    }

    /// 转发平台事件（按下 / 抬起；见 [`Interactive::dispatch`]）。
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.interactive.dispatch(app, event)
    }

    /// 运行时禁用。
    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        self.interactive.set_disabled(app, disabled)
    }

    /// 运行时加载态。
    pub fn set_loading(&self, app: &mut App, loading: bool) -> bool {
        self.interactive.set_loading(app, loading)
    }

    /// 运行时选中态。
    pub fn set_selected(&self, app: &mut App, selected: bool) -> bool {
        self.interactive.set_selected(app, selected)
    }

    /// 释放信号作用域与 Action handler。
    pub fn release(&self, app: &mut App) -> usize {
        self.interactive.release(app)
    }
}

impl UnifiedSizable for Button {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.size = size;
        self
    }
}

/// 按钮铺满父行并居中内容（旧 alert / dialog 按钮行的 `flex_1 + justify_center`）。
///
/// 裁决（P2-4a2）：按钮行的 flex 布局归包裹层（`kit::alert` / `kit::dialog`），[`Button`]
/// 本身不新增 flex 拉伸语义。实现只有这一份，两个容器共用（P2-4a3 去重；旧 `kit::alert` 与
/// `kit::dialog` 各有一份同名同实现的私有副本）。
pub(crate) fn stretch_button(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<(), EngineError> {
    let tree = app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    let mut style = tree.get(node)?.style().clone();
    style.inner.flex_grow = 1.0;
    style.inner.flex_shrink = 1.0;
    style.inner.flex_basis = Dimension::length(0.0);
    style.inner.justify_content = JustifyContent::CENTER;
    tree.set_style(node, style)
}

/// `danger` 的四状态角色配方（主题没有 danger 交互配方，这里用状态角色拼）。
fn danger_palette() -> RolePalette {
    RolePalette {
        normal: StateRoles {
            background: Some(Role::StatusDangerBase),
            foreground: Role::FgOnBrand,
            border: Some(Role::StatusDangerBase),
        },
        hover: StateRoles {
            background: Some(Role::StatusDangerBase),
            foreground: Role::FgOnBrand,
            border: Some(Role::StatusDangerBorder),
        },
        active: StateRoles {
            background: Some(Role::StatusDangerBorder),
            foreground: Role::FgOnBrand,
            border: Some(Role::StatusDangerBorder),
        },
        disabled: StateRoles {
            background: Some(Role::SurfaceControl),
            foreground: Role::FgDisabled,
            border: Some(Role::SurfaceControl),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use ui_core::{
        a11y::A11yRole,
        event::{ActionSource, InputEvent, Modifiers, PointerButton},
        math::Point,
        node::{id::NodeId, tree::NodeKind},
        paint::color::{Color, ColorResolver},
        test_support::app::TestApp,
    };

    use super::{Button, ButtonVariant};
    use crate::{
        kit::{icon::IconKind, size::UnifiedSizable},
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

    struct Harness {
        app: TestApp,
        button: Button,
        node: NodeId,
    }

    fn harness(configure: impl FnOnce(Button) -> Button) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let button = configure(Button::new(app.app_mut(), theme, "button").label("Save"));
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = button.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let _ = button.sync(app.app_mut());
        let _ = app.drain().expect("frame");
        Harness { app, button, node }
    }

    fn patch(harness: &Harness) -> ui_core::paint::style::StylePatch {
        let window = harness.app.window_id();
        harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(harness.node)
            .expect("node")
            .style_patch()
            .clone()
    }

    fn resolved(color: Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    fn light_theme() -> Theme {
        Theme::of(Appearance::Light)
    }

    /// 五种变体各自映射到正确的配方：前四个用主题交互配方，danger 用状态角色。
    #[test]
    fn every_button_variant_maps_to_its_theme_recipe() {
        let components = light_theme().components().interactive;
        let cases = [
            (ButtonVariant::Primary, components.filled.normal, "primary"),
            (
                ButtonVariant::Secondary,
                components.tonal.normal,
                "secondary",
            ),
            (ButtonVariant::Outline, components.outline.normal, "outline"),
            (ButtonVariant::Ghost, components.ghost.normal, "ghost"),
        ];
        for (variant, recipe, name) in cases {
            let harness = harness(|button| button.variant(variant));
            assert_eq!(
                patch(&harness).background.map(resolved),
                Some(resolved(recipe.background)),
                "{name} 的背景"
            );
            assert_eq!(
                patch(&harness).foreground.map(resolved),
                Some(resolved(recipe.foreground)),
                "{name} 的前景"
            );
        }
        let harness = harness(|button| button.danger());
        let theme = light_theme();
        assert_eq!(
            patch(&harness).background.map(resolved),
            Some(resolved(theme.role(Role::StatusDangerBase))),
            "danger 的背景是状态危险色"
        );
        assert_eq!(
            patch(&harness).foreground.map(resolved),
            Some(resolved(theme.role(Role::FgOnBrand))),
            "danger 的文字是 on-brand 色"
        );
    }

    /// 图标 + 文字：图标是 E7 图片节点，文字是文本节点，顺序是「图标 → 文字」。
    #[test]
    fn a_button_with_an_icon_draws_the_icon_before_the_label() {
        let harness = harness(|button| button.icon(IconKind::Check).large());
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        let children = tree.get(harness.node).expect("node").children();
        assert_eq!(children.len(), 2, "图标 + 文字");
        assert!(matches!(
            tree.get(children[0]).expect("icon").kind(),
            NodeKind::Image(_)
        ));
        match tree.get(children[1]).expect("label").kind() {
            NodeKind::Text(content) => {
                assert_eq!(content.text, "Save");
                assert_eq!(
                    content.style.size,
                    crate::kit::size::UnifiedSize::Large.text_size()
                );
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    /// 尺寸阶梯：大号按钮高 40、水平内边距 10、字号 18，图标 16。
    #[test]
    fn the_size_ladder_reaches_layout_text_and_icon() {
        let harness = harness(|button| button.icon(IconKind::Check).large());
        let bounds = harness.app.probe().bounds(harness.node).expect("bounds");
        assert_eq!(bounds.size.height, 40.0, "大号按钮高 40");
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        let children = tree.get(harness.node).expect("node").children();
        let icon_style = tree.get(children[0]).expect("icon").style();
        assert_eq!(
            icon_style.inner.size.width,
            taffy::style::Dimension::length(16.0),
            "大号图标边长 16"
        );
        let label_style = tree.get(children[1]).expect("label").style();
        assert_eq!(
            label_style.inner.size.height,
            taffy::style::Dimension::auto(),
            "文字节点高度由行高决定"
        );
    }

    /// 禁用：不可交互 + `Disabled` 配方；指针点击不触发回调。
    #[test]
    fn a_disabled_button_does_not_invoke_its_callback() {
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let mut harness = harness(move |button| {
            button.disabled(true).on_click(move |_| {
                counter.set(counter.get() + 1);
            })
        });
        assert_eq!(patch(&harness).interactive, Some(false));
        let point = Point::new(
            harness
                .app
                .probe()
                .bounds(harness.node)
                .expect("bounds")
                .origin
                .x
                + 10.0,
            harness
                .app
                .probe()
                .bounds(harness.node)
                .expect("bounds")
                .origin
                .y
                + 10.0,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert_eq!(calls.get(), 0, "禁用按钮不得触发 on_click");
    }

    /// 加载态：不可交互 + a11y `busy`。
    #[test]
    fn a_loading_button_is_busy_and_not_interactive() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let button = Button::new(app.app_mut(), theme, "loading")
            .label("Save")
            .loading(true);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = button.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert_eq!(record.role, A11yRole::Button);
        assert!(record.busy, "加载中的按钮必须宣告 busy");
        assert!(record.disabled, "加载中的按钮不可交互");
    }

    /// a11y：按钮声明 `Button` 角色与可见文字（变异：删掉角色声明 → 本测试红）。
    #[test]
    fn a_button_declares_the_button_role_and_its_label() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let button = Button::new(app.app_mut(), theme, "a11y").label("Save");
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = button.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("Save"));
    }

    /// icon-only 按钮用 `a11y_label` 补可访问名（旧缺口：可见文字为空 ⇒ AT 读「无名按钮」）。
    #[test]
    fn an_icon_only_button_reports_its_a11y_label() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let button = Button::new(app.app_mut(), theme, "icon-only")
            .icon(IconKind::X)
            .a11y_label("Close");
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = button.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(
            record.label_text(),
            Some("Close"),
            "icon-only 按钮必须有可访问名"
        );
    }

    /// 点击回调：指针点击触发，程序化禁用派发被守卫拦下。
    #[test]
    fn a_click_runs_the_callback_and_a_disabled_programmatic_action_does_not() {
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let mut harness = harness(move |button| {
            button.on_click(move |_| {
                counter.set(counter.get() + 1);
            })
        });
        let window = harness.app.window_id();
        let bounds = harness.app.probe().bounds(harness.node).expect("bounds");
        let point = Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert_eq!(calls.get(), 1, "指针点击触发 on_click");

        assert!(harness.button.set_disabled(harness.app.app_mut(), true));
        let action = harness.button.action_identity().expect("自动动作");
        let _ = harness.app.app_mut().dispatch_action(
            window,
            harness.node,
            action,
            ActionSource::Programmatic,
        );
        assert_eq!(calls.get(), 1, "禁用后程序化 click 不再触发");
    }

    /// 拼接：非首 / 非尾按钮只保留外角圆角（frame 的 quad 圆角）。
    #[test]
    fn joined_buttons_square_their_shared_corners() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let first = Button::new(app.app_mut(), theme.clone(), "join-0")
            .label("A")
            .joined(0, 2);
        let second = Button::new(app.app_mut(), theme, "join-1")
            .label("B")
            .joined(1, 2);
        first.build(app.app_mut(), window, root).expect("first");
        second.build(app.app_mut(), window, root).expect("second");
        let record = app.drain().expect("frame").clone();
        let frame = record.frame.as_ref().expect("rendered");
        let radii: Vec<[f32; 4]> = frame.quads.iter().map(|quad| quad.corner_radius).collect();
        assert!(
            radii.contains(&[4.0, 0.0, 0.0, 4.0]),
            "首按钮保留左外角：{radii:?}"
        );
        assert!(
            radii.contains(&[0.0, 4.0, 4.0, 0.0]),
            "尾按钮保留右外角：{radii:?}"
        );
    }
}
