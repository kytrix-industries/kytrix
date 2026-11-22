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
//! - **延迟（E10 一次性定时器）**：`App::after_scoped(window, trigger, delay, ..)` 在到期时于
//!   UI 线程回调一次（引擎时钟、等待期零帧 / 零唤醒）；回调只把「到点」标记置位，状态仍由
//!   [`Tooltip::sync`] 在下一轮钩子里推进。不再用「0 → 1 动画」表达延迟：那会在延迟期每帧
//!   一个动画 tick，而且终值在钩子之后写入、真实泵序下可能永远等不到（P2-4b R02）。
//! - **定位 / 淡入淡出 / 关闭**：全部复用 [`Overlay`]（`Band::Tooltip` 层带、`Placement` 12 向、
//!   `fade`、`Scrim` 不启用）。淡出收尾由 kit 的 [`AnimationRouter`]（E10 完成回调）负责：
//!   `Tooltip::new` 必须拿到组合根**共享的** router（与 overlay 同一个单槽）。
//! - **模态抑制（E12 / R07）**：模态浮层打开后，引擎把作用域外的 `hover` / `focus` 读作
//!   `None`（`App::modal_scope` 是公开查询面）。本模块的激活判据只读这两个镜像 —— **不自行
//!   推导**「背景是否被模态挡住」，因此背景触发器不会在模态之上打开提示，而模态内部（含
//!   经 `opener` 链属于该模态的弹出层）的触发器照常工作。
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
//! - 淡入淡出是**整组不透明度**（E13 经 [`Overlay`] 接入）：面板背景 + 文本 + 快捷键在同一棵
//!   子树里，一次动画全部跟随，不需要逐节点读 `Overlay::progress`；实现仍暴露 `progress`
//!   （调试 / 应用自绘内容用）。
//! - 内容变化（[`Tooltip::set_content`]）在提示显示期间 = 先完整收起（淡出），再按**新内容**
//!   重开：`Overlay::open` 的同窗重入只换锚点、不重建面板内容，所以不能靠它撤销量变化
//!   （旧实现靠「撤回内容关、恢复内容不复活」的 hover 语义；引擎没有 per-node hover 回调，
//!   统一由 `sync` 的激活判据驱动）。**撤回内容（`None`）后在淡出窗口内恢复 `Some(new)` 也走
//!   同一条重建路径**（`pending_restart`；P2-4b3 N1：旧实现会在 Idle 分支撤销关闭、让屏上留在
//!   旧文本）。
//! - a11y：提示的 `Tooltip` 角色 + 可访问名（文本 + 快捷键）**显式声明在引擎浮层包装节点**上
//!   （E12：包装不再自动 `Dialog`；内容根保持通用容器，避免 AT 读两层同名角色）；**没有**
//!   `aria-describedby` 关系（引擎 `A11yOverride` 无 description 关系通道），触发器与提示是
//!   两棵子树里的节点。
//!
//! # 用法
//!
//! ```ignore
//! let layer = Layer::new();              // 组合根唯一；菜单 / 对话框 / 提示都登记到它
//! let router = AnimationRouter::new();   // 组合根唯一；overlay / tooltip 共用完成路由（E10 单槽）
//! let tip = Tooltip::new(app, theme.clone(), layer.clone(), router.clone(), "save",
//!     TooltipOptions::default());
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
    app::{App, TimerId},
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::SignalScope, tree::NodeTree},
    paint::style::StylePatch,
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
    /// 内容宽度上限（逻辑像素，**不含**面板内边距；面板总宽 = 上限 + 2 × 内边距）。
    ///
    /// 超过上限的文本按 `TextWrap::Wrap` 换行（面板高度随之增长）。`f32::INFINITY` / 非有限
    /// 值 / `<= 0` = 不限制（面板按内容自然宽度收缩）。
    ///
    /// 默认 `320.0`：旧 `base_ui` 的提示内容固定 `max_w(px(320.0))`（W3a F6 的视觉等价面），
    /// 长句在窄 pane 里的换行宽度因此与旧实现一致。
    pub max_width: f32,
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
            // 旧实现的内容宽度上限（`max_w(px(320.0))`，W3a F6）。
            max_width: 320.0,
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

    /// 内容宽度上限（不含面板内边距；`f32::INFINITY` = 不限制）。
    #[must_use]
    pub fn max_width(mut self, max_width: f32) -> Self {
        self.max_width = max_width;
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
    /// 延迟的一次性定时器（E10 `App::after_scoped`；等待期零帧 / 零唤醒）。
    delay_timer: Cell<Option<TimerId>>,
    /// 定时器到点标记：回调只置位，打开动作仍由 [`Tooltip::sync`] 推进（状态机单一入口）。
    delay_ready: Cell<bool>,
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
    /// `router` 必须是**组合根的共享 [`AnimationRouter`]**（与所有 overlay 同一个单槽）：
    /// 淡出收尾依赖 E10 完成回调，各自注册会互相覆盖（E10-R02）。**kit 路由是应用唯一的完成
    /// 回调注册者（B3）**：应用不得再调 `App::on_animation_complete`；需要多消费者时扩展 kit
    /// 路由（见 [`AnimationRouter`] 的文档），不加引擎 API。
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
                // 提示不是模态弹层：外部点击必须落到下层控件，Esc（引擎 `Dismiss`）仍可关。
                dismiss: DismissPolicy::Explicit,
                modal: false,
                // 提示不抢焦点、关闭也不改焦点。
                restore_focus: false,
                band: Band::Tooltip,
                slot: crate::kit::layer::LayerSlot::Exclusive,
                surface: OverlaySurface::tooltip(),
                scrim: None,
                // 角色 / 可访问名挂在引擎浮层包装节点上（每次 show 按当前内容刷新）。
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
    /// 零工作、不出帧（I3）。延迟到点由 E10 定时器回调置位（下一轮钩子仍在帧之前），淡出收尾
    /// 由 E10 完成回调在动画终值那一帧完成 —— 本方法不再观测动画进度。
    pub fn sync(&self, app: &mut App) -> bool {
        // 浮层自身的帧同步始终跑：引擎侧 Esc 关闭的探测 / 定位 / 层带仲裁靠它。
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
                } else if !self.inner.overlay.is_open(app) {
                    // 只有旧浮层**真的关掉之后**才清标记（B1）：淡出窗口内离开触发器时若清掉，
                    // 随后重新进入会走 `Overlay::open` 的同窗重入（只换锚点、不重建内容）——
                    // 屏上复活的是旧文本，而 `shown` 已经是新内容。`pending_restart` 只能在
                    // 「旧浮层已关闭」或 `reset()`（触发器丢失 / 引擎侧关闭）时清。
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
                    // 收起；若屏上内容已经和「当前应有内容」不一致（撤回 / 变了），淡出窗口内
                    // 不得被 Idle 分支的撤销关闭复活旧面板 —— 等完整关闭后按当前内容重建
                    // （N1：`set_content(None)` 后在淡出窗口内恢复 `Some(new)` 走同一条重建路径）。
                    self.hide(app);
                    self.inner.pending_restart.set(withdrawn || content_changed);
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
        self.cancel_delay(app);
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
    ///
    /// R07（E12）：模态浮层打开后，`App::hovered` / `App::focused` 已经把**作用域外**的输入
    /// 读作 `None`（`App::modal_scope` 是公开查询面，判据由引擎唯一提供）。本模块只读这两个
    /// 镜像，**不自行推导**「背景是否被模态挡住」—— 背景触发器因此不会在模态之上打开提示，
    /// 而模态内部（含经 `opener` 链属于该模态的弹出层）的触发器照常工作。
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

    /// 开始延迟（E10 一次性定时器；`delay == 0` ⇒ 直接打开）。
    ///
    /// 定时器绑在**触发器节点**上（`after_scoped`）：触发器卸载时引擎自动取消；指针离开 /
    /// 内容撤回 / 改绑由 [`Tooltip::cancel_delay`] 显式取消。等待期不请求帧、不进连续帧（I3）。
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
                // 到期后句柄失效（一次性）；回调只置位，打开动作仍由 `sync` 推进。
                inner.delay_timer.set(None);
                inner.delay_ready.set(true);
            }
        });
        match registered {
            Ok(id) => self.inner.delay_timer.set(Some(id)),
            // 窗口 / 触发器在登记瞬间已失效：复位（不会再有显示对象）。
            Err(_) => self.reset(app),
        }
    }

    /// 打开提示浮层（内容按当前 `content` 重建；a11y 角色 / 可访问名按当前内容刷新到
    /// 引擎浮层包装节点上）。
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

    /// 收起提示（播浮层的淡出；E10 完成回调在同一帧收尾卸载）。
    fn hide(&self, app: &mut App) {
        if self.inner.phase.get() == Phase::Shown {
            let _ = self.inner.overlay.close(app);
        }
        self.inner.phase.set(Phase::Idle);
        *self.inner.shown.borrow_mut() = None;
        self.cancel_delay(app);
    }

    /// 收起（不播淡出）+ 清空延迟计时。
    fn reset(&self, app: &mut App) {
        self.inner.phase.set(Phase::Idle);
        *self.inner.shown.borrow_mut() = None;
        self.inner.pending_restart.set(false);
        self.cancel_delay(app);
        let _ = self.inner.overlay.close_now(app);
    }

    /// 取消在飞的延迟定时器（幂等；到期 / 已取消返回 false 不需要处理）。
    ///
    /// **冗余防御（B5）**：`start_delay`（重登记前）/ [`Tooltip::hide`] / [`Tooltip::reset`] 三处
    /// 都会调用。行为契约只有一条——「离开 / 撤回 / 改绑后旧的到期点不得生效」；真正兜底的是
    /// 重登记前的取消 + `delay_ready` 复位，单点删除不改变可观察行为（测试按行为断言，不按
    /// 取消点断言）。
    fn cancel_delay(&self, app: &mut App) {
        self.inner.delay_ready.set(false);
        if let Some(id) = self.inner.delay_timer.take() {
            let _ = app.cancel_timer(&id);
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
///
/// `Tooltip` 角色 / 可访问名声明在**引擎浮层包装节点**上（[`Overlay::set_a11y`]，E12：包装不再
/// 自动 `Dialog`），内容根保持通用容器 —— 两层同名角色会让 AT 读两遍（E12-R1）。
///
/// 宽度：`max_width` 是**内容行**的上限（面板总宽 = 上限 + 面板内边距），文本节点
/// `min_width_0` 可收缩 ⇒ 超宽文本按 `TextWrap::Wrap` 换行、快捷键 `flex_none` 保持整块；
/// 非有限 / `<= 0` 的上限视为不限（[`TooltipOptions::max_width`]）。
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
    //! 行为断言：延迟（E10 定时器）/ hover 与 focus 触发 / 离开关闭 / 12 向定位复用 /
    //! 文本与快捷键渲染 / 不可交互与 a11y（E12 包装角色）/ 撤回内容 / 模态抑制（R07）/
    //! 淡出窗口内的内容重建（N1/N2）。

    use std::{cell::Cell, collections::BTreeSet, rc::Rc, time::Duration};

    use smol_str::format_smolstr;
    use ui_core::{
        a11y::A11yRole,
        error::EngineError,
        event::{Action, InputEvent, KeyCode, Modifiers},
        math::{Point, Rect},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
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
        style_builder::StyleBuilder,
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
            // 首帧先把布局跑出来：指针命中依赖布局盒（否则 hover 命中不到触发器）。
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

        /// 真实泵序的一轮：钩子（`Tooltip::sync` = 组合根 `on_frame`）→ 帧。
        /// E10 的定时器 / 完成回调在帧入口或帧内派发，因此断言在 `pump` 之后。
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

        /// 模态对话框（kit `Overlay`；fade=false，便于断言 hover 判定本身）。
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
            // 首帧按零尺寸落位，`sync` 用实测尺寸纠位并揭开面板（不可见的节点不参与命中）。
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
        // R02（E10）：延迟是**一次性定时器**，不是 0→1 动画 —— 等待期零帧、零动画；定时器在
        // 到期帧的入口派发（回调只置位），下一轮钩子（`sync`）才打开。
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

        // 走一半：仍然不可见，而且等待期这一帧是 Idle（I3：定时器不请求帧、不进连续帧）。
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

        // 走完：到期帧入口派发定时器（只置位），下一轮钩子打开。
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
        // 先让淡入完成（E10 完成回调在帧内清动画句柄，不关闭）。
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
        // 真实泵序：淡出到终值的那一帧由 E10 完成回调卸载。**帧精确**（B2）：断言绑在
        // 「`animations_running` 变 false 的那一轮 `drain` 之后」（该轮钩子已跑过，不补 sync）；
        // 旧 P0 形态（完成回调不卸载、等下一轮 sync 观测终值）在这里必红。
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

    /// **P2-10**（W3a F6）：提示内容宽度上限。
    ///
    /// 旧 `base_ui` 的提示内容固定 `max_w(px(320.0))`（面板再 `p_1()`）；迁移后的 kit 没有宽度
    /// 入口，长句在窄 pane 里的换行宽度与旧实现不同。`TooltipOptions::max_width` 恢复该能力：
    /// 默认 320（面板总宽 = 上限 + 2 × 4 面板内边距）、可调小（换行更多 ⇒ 更高）、
    /// `f32::INFINITY` = 不限（单行撑开、更矮）。
    ///
    /// 变异防线：`build_content` 忽略 `max_width`（不给内容行加 `max_width`）⇒ 面板宽 = 单行
    /// 全文宽，三条宽度断言全红。
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
        // R12/E12：`Tooltip` 角色 + 可访问名（文本 + 快捷键）显式声明在引擎浮层**包装节点**上
        // （E12 之后包装不再自动 Dialog）；内容根保持通用容器。
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
        // R05/N2：显示中改内容必须**先完整淡出、关闭后再按新内容重建**。测试必须走真实淡出
        // 窗口（淡入先完成、内容变化后在淡出窗口内继续 sync + tick + drain）——旧写法
        // 「advance 后不 drain ⇒ 淡入未 tick ⇒ progress == 0 ⇒ close_now 立即卸载」只覆盖
        // 立即关闭路径，M4 变异（内容变化不重建）漏检。
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::ZERO,
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        // 淡入完成（E10 完成回调在帧内收尾）：只有 progress == 1 时 `close` 才走淡出而不是立即卸载。
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

        // 真实淡出窗口内：内容变化标记必须阻止 Idle 分支撤销关闭（否则旧面板复活）。
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

        // 淡出走完（完成回调卸载）→ 下一轮 Idle 分支按新内容重开。
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
        // N1（P2-4b 审查轮次 2）：`set_content(None)` 后在淡出窗口内恢复 `Some(new)`，旧路径在
        // Idle 分支撤销关闭 ⇒ 屏上仍是旧面板文本，而 `shown` 已是新内容，之后不再重建。
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

        // 先撤回内容（开始淡出），再在淡出窗口内恢复新内容。
        assert!(harness.tip.set_content(None));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：撤回后进入淡出窗口"
        );
        assert!(harness.tip.set_content(Some(TooltipContent::new("新建"))));

        // 淡出窗口内不得复活旧面板。
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

        // 旧浮层关闭后按恢复的新内容重建。
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
        // B1（P1）：N1 的同类变体。内容变化 → 旧面板淡出（`pending_restart`）→ 淡出窗口内**离开
        // 触发器再重新进入**。旧实现在 `Phase::Idle` 的 else 分支无条件清 `pending_restart`，
        // 重新进入会走 `Overlay::open` 的同窗重入（只换锚点、不重建内容）⇒ 屏上复活旧面板，
        // 而 `shown` 已是新内容，之后不再重建。修复：只有旧浮层真的关闭后才清标记。
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

        // 显示中改内容：旧面板开始淡出，等它完整关闭后按新内容重建（`pending_restart`）。
        assert!(harness.tip.set_content(Some(TooltipContent::new("新建"))));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：仍在淡出窗口内"
        );

        // 淡出窗口内离开触发器（不推进时钟 ⇒ 淡出动画不 tick，节点一定还在）。
        harness.hover(Point::new(10.0, 10.0));
        let _ = harness.sync();
        assert!(
            harness.tip.overlay().overlay_node().is_some(),
            "前置：离开后仍在淡出窗口内"
        );
        // 淡出窗口内重新进入：不得撤销关闭复活旧面板（B1 变异：这里 `is_visible` 会变 true）。
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

        // 旧浮层由 E10 完成回调卸载 → 下一轮 Idle 分支按新内容重建。
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
        // E10 定时器生命周期（B5：声明收敛到**可验证**的口径）：「离开后旧的到期点不得生效」
        // 是行为契约；实现里 `start_delay`（重登记前）/ `hide` / `reset` 三处取消互为冗余，
        // 单删任一处行为仍正确（旧测试名「必须取消」在单点变异下不成立，已改名）。本测试断言
        // 可观察行为：走过原到期点不显示、重新进入后重新计满延迟（不沿用旧到期点）。
        let mut harness = Harness::new(TooltipOptions {
            delay: Duration::from_millis(500),
            ..TooltipOptions::default()
        });
        harness.hover(harness.trigger_center());
        let _ = harness.sync();
        harness.hover(Point::new(10.0, 10.0));
        let _ = harness.sync();
        assert!(!harness.tip.is_visible(harness.app.app()));

        // 离开后即使走过原到期点也不得显示（旧到期点已失效；任一取消点生效即可）。
        harness.advance(Duration::from_millis(700));
        let _ = harness.app.drain().expect("帧");
        let _ = harness.sync();
        assert!(
            !harness.tip.is_visible(harness.app.app()),
            "离开后旧的到期点不得生效（否则提示会在没有悬停时弹出）"
        );

        // 重新进入：重新计满 500 ms 才显示。
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
        // R07/E12：模态作用域由引擎唯一提供（`App::modal_scope`）；作用域外的 hover / focus
        // 读作 `None`，背景触发器因此不会在模态之上打开提示。kit 不自行推导模态作用域。
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

        // 指针移到**背景**触发器上：模态作用域存在时，提示不得在模态之上打开。
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
        // R07 的反向保证：模态**内部**（含经 opener 链属于该模态的弹出层）的触发器照常工作；
        // `Band::Tooltip` 最高 ⇒ 提示画在对话框之上。
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
        // 提示绑定到模态内的触发器（背景触发器仍在树里，但不参与这次激活）。
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
