//! tooltip：hover / focus 触发、延迟、复用 overlay 定位、文本 + 快捷键渲染、不可交互、a11y
//! （旧 `crate::tooltip` 的引擎化移植）。
//!
//! # 旧实现的事实
//!
//! 旧 `Tooltip`（`crates/base_ui/src/tooltip.rs`，386 行 / 4 测试）只是一个 `Overlay` 包装：
//! trigger 上 `on_hover` 写 `Entity<bool>`，内容为 `None` 时**关闭**已打开的提示（「调用方撤回
//! 内容 = 关闭」），面板用 `POPUP_PRIORITY + 2` 画在所有弹层之上。它**没有延迟**，也没有
//! focus 触发（纯 hover）。
//!
//! # 引擎化后的拆解
//!
//! - **触发**：引擎的 hover 是全局镜像（`App::hovered`，不进 `ReadCtx`），焦点是
//!   `App::focused`。本模块在 [`Tooltip::sync`] 里把「指针/焦点落在触发器子树内」当作激活判据，
//!   与 `scroll_bar` 的悬停镜像同一机制（hover 变化本身会请求一帧，因此 sync 一定会被叫到）。
//! - **延迟**：引擎没有墙钟定时器 / `defer` API。延迟用「0 → 1 的线性动画」表达：`App::animate`
//!   在 `delay` 期间按显示节拍推进 `Transient<f32>`，`sync` 读到 1 才真正打开浮层。代价是延迟
//!   期间每帧一个动画 tick（连续帧由动画驱动，不是空闲出帧）；延迟结束后动画停止、回到 I3。
//! - **定位 / 淡入淡出 / 关闭**：全部复用 [`Overlay`]（`Band::Tooltip` 层带、`Placement` 12 向、
//!   `fade`、`Scrim` 不启用）。
//! - **共享组合根的 Layer**：`Tooltip::new` 必须拿到组合根（或至少与菜单 / 弹层同一个）的
//!   [`Layer`]。提示是 `Band::Tooltip` 层带里的一员，只有和 context menu / dialog 登记在
//!   **同一张层带表**里，旧实现的 `POPUP_PRIORITY + 2`（提示画在菜单之上）才成立；每实例私有
//!   Layer 会让 `Band::Tooltip` 只和自己比较（R09）。
//! - **不可交互**：提示内容只有文本节点（没有动作、没有 `tab_stop`），浮层非模态（不抢焦点）；
//!   指针在提示面板上会**离开触发器** ⇒ `sync` 立即收起提示（与旧实现「hover 离开即关」同义）。
//! - **关闭策略**：`DismissPolicy::Explicit` —— 提示不是可被「点外部」关闭的模态弹层：外部点击
//!   必须落到下面的控件上（旧实现对 tooltip 没有 `on_mouse_down_out`）。
//!
//! # 与旧实现的差异（已接受，见 spec-delta）
//!
//! - 新增 **500 ms 延迟**（旧实现无延迟；`TooltipOptions::delay` 可调，`Duration::ZERO` = 立即）。
//! - 新增 **focus 触发**（键盘用户的提示不能只靠 hover）。
//! - 内容变化（[`Tooltip::set_content`]）在提示显示期间 = 先完整收起（淡出），再按**新内容**
//!   重开：`Overlay::open` 的同窗重入只换锚点、不重建面板内容，所以不能靠它撤销量变化
//!   （旧实现靠「撤回内容关、恢复内容不复活」的 hover 语义；引擎没有 per-node hover 回调，
//!   统一由 `sync` 的激活判据驱动）。
//! - a11y：提示内容根声明 `A11yRole::Tooltip` + 可访问名；**没有** `aria-describedby` 关系
//!   （引擎 `A11yOverride` 无 description 关系通道），触发器与提示是两棵子树里的节点。
//!
//! # 用法
//!
//! ```ignore
//! let layer = Layer::new();   // 组合根唯一；菜单 / 对话框 / 提示都登记到它
//! let tip = Tooltip::new(app, theme.clone(), layer.clone(), "save", TooltipOptions::default());
//! tip.set_content(Some(TooltipContent::new("保存").shortcut("Ctrl+S")));
//! tip.attach(app, window, save_button_node);
//! // 组合根的 on_frame 钩子里：
//! tip.sync(app);
//! ```

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    interaction::{AnimatedValue, AnimationId, AnimationSpec, Easing},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{SignalScope, Transient},
        tree::NodeTree,
    },
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        layer::{Band, Layer},
        overlay::{Anchor, DismissPolicy, Overlay, OverlayOptions, OverlaySurface, Placement},
    },
    style_builder::StyleBuilder,
    theme::{components::ComponentTokens, handle::ThemeHandle, role::Role},
};

/// 提示内容：一行文本 + 可选快捷键。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TooltipContent {
    /// 提示文本。
    pub text: SmolStr,
    /// 快捷键（`Ctrl+S` / `⌘K` 之类，已由调用方按平台格式化）。
    pub shortcut: Option<SmolStr>,
}

impl TooltipContent {
    /// 纯文本提示。
    #[must_use]
    pub fn new(text: impl Into<SmolStr>) -> Self {
        Self {
            text: text.into(),
            shortcut: None,
        }
    }

    /// 追加快捷键。
    #[must_use]
    pub fn shortcut(mut self, shortcut: impl Into<SmolStr>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// a11y 可访问名（文本 + 快捷键）。
    #[must_use]
    pub fn label(&self) -> SmolStr {
        match &self.shortcut {
            Some(shortcut) => format_smolstr!("{text}（{shortcut}）", text = self.text),
            None => self.text.clone(),
        }
    }
}

/// 提示参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TooltipOptions {
    /// 定位（复用 [`crate::kit::overlay::Placement`] 的 12 向）。
    pub placement: Placement,
    /// 与触发器的间隙。
    pub gap: f32,
    /// 悬停 / 聚焦后多久显示（`Duration::ZERO` = 立即）。
    pub delay: Duration,
    /// 是否淡入淡出。
    pub fade: bool,
}

impl Default for TooltipOptions {
    fn default() -> Self {
        Self {
            // 旧 `Tooltip` 默认 `Placement::Bottom`（下方居中）。
            placement: Placement::BOTTOM,
            gap: 4.0,
            // 旧实现没有延迟；新增的 500 ms 是提示类控件的通用默认（可调到 0 立即显示）。
            delay: Duration::from_millis(500),
            fade: true,
        }
    }
}

impl TooltipOptions {
    /// 立即显示（无延迟）。
    #[must_use]
    pub fn immediate(mut self) -> Self {
        self.delay = Duration::ZERO;
        self
    }

    /// 指定延迟。
    #[must_use]
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// 提示的生命周期阶段。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    /// 未激活。
    #[default]
    Idle,
    /// 激活，延迟计时中。
    Waiting,
    /// 已打开。
    Shown,
}

struct TooltipInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: TooltipOptions,
    scope: SignalScope,
    overlay: Overlay,
    /// 延迟计时（`Transient<f32>`：`0` → `1`，由 `App::animate` 推进；不进依赖图）。
    elapsed: Transient<f32>,
    delay_animation: Cell<Option<AnimationId>>,
    phase: Cell<Phase>,
    window: Cell<Option<WindowId>>,
    trigger: Cell<Option<NodeId>>,
    content: RefCell<Option<TooltipContent>>,
    /// 当前已显示的提示内容（内容变化 ⇒ 收起后按新内容重开）。
    shown: RefCell<Option<TooltipContent>>,
    /// 内容变化导致的「等旧浮层关闭后重开」标记（见 [`Tooltip::sync`] 的 `Shown` / `Idle`）。
    pending_restart: Cell<bool>,
}

/// 提示句柄（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct Tooltip {
    inner: Rc<TooltipInner>,
}

impl Tooltip {
    /// 建提示状态（关闭态）。之后 [`Tooltip::set_content`] + [`Tooltip::attach`]。
    ///
    /// `layer` 必须是**组合根的共享 Layer**（与菜单 / 对话框同一个）：提示要在
    /// [`Band::Tooltip`] 层带上参与跨组件 z 序仲裁（R09）。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        key: impl Into<SmolStr>,
        options: TooltipOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        let overlay = Overlay::new(
            app,
            theme.clone(),
            layer,
            format_smolstr!("{key}:overlay"),
            OverlayOptions {
                placement: options.placement,
                gap: options.gap,
                margin: 4.0,
                flip: true,
                // 提示不是模态弹层：外部点击必须落到下层控件，Esc（引擎 `Dismiss`）仍可关。
                dismiss: DismissPolicy::Explicit,
                modal: false,
                // 提示不抢焦点、关闭也不改焦点。
                restore_focus: false,
                band: Band::Tooltip,
                surface: OverlaySurface::tooltip(),
                scrim: None,
                // 角色 / 可访问名挂在内容根上（内容每次打开按当前文本重建）。
                a11y: Default::default(),
                fade: options.fade,
            },
        );
        Self {
            inner: Rc::new(TooltipInner {
                elapsed: app.transient_scoped(scope, 0.0),
                scope,
                key,
                theme,
                options,
                overlay,
                delay_animation: Cell::new(None),
                phase: Cell::new(Phase::Idle),
                window: Cell::new(None),
                trigger: Cell::new(None),
                content: RefCell::new(None),
                shown: RefCell::new(None),
                pending_restart: Cell::new(false),
            }),
        }
    }

    /// 提示键（调试 id 前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 绑定触发器节点与窗口（重复调用 = 改绑；改绑时收起已显示的提示）。
    pub fn attach(&self, app: &mut App, window: WindowId, trigger: NodeId) {
        if self.inner.trigger.get() != Some(trigger) || self.inner.window.get() != Some(window) {
            self.hide(app);
        }
        self.inner.window.set(Some(window));
        self.inner.trigger.set(Some(trigger));
    }

    /// 设置提示内容（`None` = 撤回提示，已显示时立即收起）。
    ///
    /// 返回 `true` = 内容变了（[`Tooltip::sync`] 会在本帧按新内容决定显示 / 收起）。
    pub fn set_content(&self, content: Option<TooltipContent>) -> bool {
        let mut current = self.inner.content.borrow_mut();
        if *current == content {
            return false;
        }
        *current = content;
        true
    }

    /// 当前内容。
    #[must_use]
    pub fn content(&self) -> Option<TooltipContent> {
        self.inner.content.borrow().clone()
    }

    /// 提示此刻是否可见（已打开且浮层仍存活）。
    #[must_use]
    pub fn is_visible(&self, app: &App) -> bool {
        self.inner.phase.get() == Phase::Shown && self.inner.overlay.is_open(app)
    }

    /// 浮层句柄（组合 / 测试用）。
    #[must_use]
    pub fn overlay(&self) -> &Overlay {
        &self.inner.overlay
    }

    /// 帧同步（组合根的 `on_frame` 钩子里调用；测试直接调）：推进延迟 / 打开 / 收起。
    ///
    /// 返回 `true` = 本帧有状态变化（或浮层自身做了工作）。稳态（未激活 / 已显示且指针未动）
    /// 零工作、不出帧（I3）。
    pub fn sync(&self, app: &mut App) -> bool {
        // 浮层自身的帧同步始终跑：淡出收尾 / 引擎侧 Esc 关闭的探测都靠它。
        let mut changed = self.inner.overlay.sync(app);
        if self.inner.phase.get() == Phase::Shown && !self.inner.overlay.is_open(app) {
            // 引擎侧关闭（Esc / `Dismiss`）：状态回到未激活。
            self.reset(app);
            return true;
        }
        let Some(trigger) = self.live_trigger(app) else {
            // 触发器已卸载 / 还没绑定：收起并复位。
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
                        // 淡出中又被激活：内容未变才撤销关闭、立即淡回（不再重新等延迟）。
                        // 内容变过（`pending_restart`）则必须等旧面板完整关闭，否则 `Overlay::open`
                        // 的同窗重入只换锚点、屏上仍是旧内容（R05）。
                        if !self.inner.pending_restart.get() {
                            changed |= self.show(app);
                        }
                    } else if self.inner.pending_restart.replace(false) {
                        // 旧面板已经关掉：按新内容重开，不再重新等延迟（提示已经显示过一次）。
                        changed |= self.show(app);
                    } else {
                        self.start_delay(app);
                        changed = true;
                    }
                } else {
                    self.inner.pending_restart.set(false);
                }
            }
            Phase::Waiting => {
                if !active || self.inner.content.borrow().is_none() {
                    self.reset(app);
                    changed = true;
                } else if self.delay_elapsed(app) >= 1.0 {
                    changed |= self.show(app);
                }
            }
            Phase::Shown => {
                let withdrawn = self.inner.content.borrow().is_none();
                let content_changed = *self.inner.content.borrow() != *self.inner.shown.borrow();
                if !active || withdrawn {
                    self.hide(app);
                    changed = true;
                } else if content_changed {
                    // 内容变了：屏上画的是旧内容，先完整收起（淡出），关掉后再按新内容重开。
                    self.hide(app);
                    self.inner.pending_restart.set(true);
                    changed = true;
                }
            }
        }
        changed
    }

    /// 释放提示与浮层的信号作用域（组合根卸载提示后调用一次；幂等）。
    pub fn release(&self, app: &mut App) {
        self.inner.overlay.release(app);
        let _ = app.release_scope(self.inner.scope);
    }

    /// 触发器节点仍存活时返回它。
    fn live_trigger(&self, app: &App) -> Option<NodeId> {
        let window = self.inner.window.get()?;
        let trigger = self.inner.trigger.get()?;
        let tree = app.window(window)?.tree();
        tree.get(trigger).ok().map(|_| trigger)
    }

    /// 指针或焦点是否落在触发器子树内（引擎的 hover / focus 都是「最具体节点」，
    /// 触发器内部的图标 / 文本会挡住触发器本身，因此要向上找祖先）。
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

    /// 开始延迟计时（`delay == 0` ⇒ 直接打开）。
    fn start_delay(&self, app: &mut App) {
        if self.inner.options.delay.is_zero() {
            self.inner.phase.set(Phase::Waiting);
            let _ = self.show(app);
            return;
        }
        self.inner.phase.set(Phase::Waiting);
        self.cancel_delay(app);
        app.write_transient(&self.inner.elapsed, 0.0);
        let Some(window) = self.inner.window.get() else {
            return;
        };
        match app.animate(
            window,
            AnimatedValue::Scalar {
                target: self.inner.elapsed,
                to: 1.0,
            },
            &[],
            AnimationSpec {
                duration: self.inner.options.delay,
                easing: Easing::Linear,
            },
        ) {
            Ok(id) => self.inner.delay_animation.set(Some(id)),
            Err(_) => app.write_transient(&self.inner.elapsed, 1.0),
        }
    }

    /// 延迟进度（`1` = 延迟已走完）。
    fn delay_elapsed(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.elapsed)
    }

    /// 打开提示浮层（内容按当前 `content` 重建）。
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
        let built = content.clone();
        let opened =
            self.inner
                .overlay
                .open(app, window, Anchor::node(trigger), move |tree, panel| {
                    build_content(tree, panel, &key, &built, &theme)
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

    /// 收起提示（播浮层的淡出；`sync` 的 `overlay.sync` 负责卸载收尾）。
    fn hide(&self, app: &mut App) {
        if self.inner.phase.get() == Phase::Shown {
            let _ = self.inner.overlay.close(app);
        }
        self.inner.phase.set(Phase::Idle);
        *self.inner.shown.borrow_mut() = None;
        self.cancel_delay(app);
        app.write_transient(&self.inner.elapsed, 0.0);
    }

    /// 收起（不播淡出）+ 清空计时。
    fn reset(&self, app: &mut App) {
        self.inner.phase.set(Phase::Idle);
        *self.inner.shown.borrow_mut() = None;
        self.inner.pending_restart.set(false);
        self.cancel_delay(app);
        app.write_transient(&self.inner.elapsed, 0.0);
        let _ = self.inner.overlay.close_now(app);
    }

    fn cancel_delay(&self, app: &mut App) {
        if let Some(id) = self.inner.delay_animation.take() {
            let _ = app.cancel_animation(&id);
        }
    }
}

/// `node` 是否是 `ancestor` 本身或其后代。
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

/// 提示内容：一行（文本 + 可选快捷键），主题色走 `bind_style.foreground`（逐节点着色）。
fn build_content(
    tree: &mut NodeTree,
    panel: NodeId,
    key: &str,
    content: &TooltipContent,
    theme: &ThemeHandle,
) -> Result<(), EngineError> {
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:tip"))
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(6.0)
                .min_width_0()
                .build(),
        )
        .a11y(A11yOverride {
            role: Some(A11yRole::Tooltip),
            label: Some(content.label()),
            ..A11yOverride::default()
        })
        .build(tree, panel)?;
    let tokens = ComponentTokens::standard();
    let size = tokens.small.font_size;
    let mut text_style = TextStyle::new(size);
    text_style.color = theme_role(theme, Role::FgOnInverse);
    let theme_text = theme.clone();
    NodeBuilder::text(content.text.clone(), text_style)
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

/// 挂载期取色（两套外观里先取浅色；真正绘制由 `bind_style` 的主题信号覆盖）。
///
/// 文本颜色在 paint 期被 `bind_style.foreground` 覆盖（引擎 `paint/pass.rs` 的文本分支），
/// 这里的静态值只用于「第一帧 bind 求值之前」与塑形（颜色不参与塑形）。
fn theme_role(theme: &ThemeHandle, role: Role) -> ui_core::paint::color::Color {
    theme
        .theme_for(crate::theme::appearance::Appearance::Light)
        .role(role)
}

#[cfg(test)]
mod tests {
    //! 行为断言：延迟 / hover 与 focus 触发 / 离开关闭 / 12 向定位复用 / 文本与快捷键渲染 /
    //! 不可交互与 a11y / 撤回内容。

    use std::{collections::BTreeSet, time::Duration};

    use smol_str::format_smolstr;
    use ui_core::{
        a11y::A11yRole,
        event::{Action, InputEvent, KeyCode, Modifiers},
        math::{Point, Rect},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{Tooltip, TooltipContent, TooltipOptions};
    use crate::{
        kit::{
            layer::{Band, Layer},
            overlay::{Anchor, DismissPolicy, Overlay, OverlayOptions, Placement},
        },
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    /// 触发器几何（与 overlay 测试同一坐标系）。
    const TRIGGER: Rect = Rect::new(300.0, 200.0, 100.0, 40.0);

    struct Harness {
        app: TestApp,
        window: WindowId,
        trigger: NodeId,
        tip: Tooltip,
        layer: Layer,
        theme: ThemeHandle,
    }

    impl Harness {
        fn new(options: TooltipOptions) -> Self {
            Self::with_layer(options, Layer::new())
        }

        fn with_layer(options: TooltipOptions, layer: Layer) -> Self {
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
            let tip = Tooltip::new(app.app_mut(), theme.clone(), layer.clone(), "tip", options);
            tip.attach(app.app_mut(), window, trigger);
            tip.set_content(Some(TooltipContent::new("保存").shortcut("Ctrl+S")));
            // 首帧先把布局跑出来：指针命中依赖布局盒（否则 hover 命中不到触发器）。
            let _ = app.drain().expect("首帧");
            Self {
                app,
                window,
                trigger,
                tip,
                layer,
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

        fn settle(&mut self) {
            for _ in 0..3 {
                self.advance(Duration::ZERO);
                let _ = self.sync();
            }
            let _ = self.app.drain().expect("帧");
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
        assert!(harness.sync(), "悬停后进入延迟计时");
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "延迟未走完不得显示"
        );

        // 走一半延迟：仍然不可见。
        harness.advance(Duration::from_millis(250));
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "延迟中途不得显示（延迟失效会在这里变红）"
        );

        // 走完延迟：打开。
        harness.advance(Duration::from_millis(300));
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()), "延迟结束后显示");
        harness.settle();
        assert!(harness.tip.overlay().overlay_node().is_some());
    }

    #[test]
    fn leaving_the_trigger_hides_the_tooltip() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()), "零延迟立即显示");

        harness.hover(Point::new(10.0, 10.0));
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "离开触发器即收起"
        );
        harness.advance(Duration::from_millis(200));
        let _ = harness.sync();
        assert!(
            harness.tip.overlay().overlay_node().is_none(),
            "淡出结束后浮层节点卸载"
        );
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
        // 水平居中：面板中心 ≈ 触发器中心。
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
    fn a_tooltip_is_not_interactive_and_declares_the_tooltip_role() {
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.app.enable_a11y_capture();
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        let _ = harness.app.drain().expect("帧");

        let panel = harness.tip.overlay().panel_node().expect("面板");
        let tip_row = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .get(panel)
            .expect("面板")
            .children()
            .first()
            .copied()
            .expect("提示内容根");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(tip_row).expect("提示 a11y");
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
        // 快速移开又移回：提示还在淡出（节点未卸载）⇒ 撤销关闭、立即淡回，不再重新等延迟。
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        // 先让淡入走完（进度 1），否则「关闭」会因不可见而立即卸载（引擎的正当语义）。
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
        // 焦点触发的提示在失焦后必须关闭：只靠 focus 激活（没有 hover 兜底）时，焦点闩锁
        // 不及时清除会让提示永久驻留（R11 的 M9 变异）。
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

        // 引擎默认键绑定：Escape → `Action::Dismiss` → 关最上层浮层（含 Explicit）。
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
        // R05：显示中改内容必须重建面板。`Overlay::open` 的同窗重入只换锚点、不重建内容，
        // 所以旧路径会让屏上停留在旧文本。
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.advance(Duration::from_millis(200));
        let _ = harness.sync();
        assert!(harness.tip.is_visible(harness.app.app()));
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

        // 淡出走完 → 旧浮层关闭 → 同一轮 Idle 分支按新内容重开。
        for _ in 0..3 {
            harness.advance(Duration::from_millis(200));
            let _ = harness.sync();
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
        // R09：提示和 context menu 必须登记进同一张层带表；否则 `Band::Tooltip` 只和自己比较，
        // 后开的菜单会画在提示之上（旧实现是 `POPUP_PRIORITY + 2`）。
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

    /// 子树里的文本载荷。
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

    /// 递归收集子树里的文本载荷。
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
