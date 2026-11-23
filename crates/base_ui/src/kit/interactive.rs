//! 通用交互状态机（旧 `crate::interactive` 的引擎化重建，P2-4a）。
//!
//! 旧实现把 hover / active 存在 旧 UI 层的 keyed state 里、把三种颜色交给 `transition()` 做隐式过渡，
//! 点击由调用方用 `InteractiveElement::on_click` 注入。新引擎只提供两样东西：节点上的 `Action`
//! 声明（指针点击 + 焦点节点的 Enter/Space 激活走同一条分发）和 `bind_style` 的 `StylePatch`。
//! 本模块因此是**应用层**的状态机：
//!
//! ```text
//! Event(PointerDown/Up) ──dispatch──▶ pressed ┐
//! App::hovered ────────sync──────────▶ hovered ├─ state ─▶ animate(mix) ─▶ bind_style(StylePatch)
//! Window::focus ───────sync──────────▶ focused ┘                          │
//!                                                                  background/foreground/border/cursor
//! ```
//!
//! # 状态与颜色
//!
//! - 状态枚举直接复用主题的 [`InteractiveState`]（`Normal` / `Hover` / `Active` / `Disabled`）与
//!   [`InteractiveVariant`]（`Filled` / `Tonal` / `Outline` / `Ghost`），颜色配方来自
//!   `ThemeHandle::components()`（**不在这里维护任何色值**）；
//! - `loading` 与 `disabled` 共用 `Disabled` 配方（禁用 = 不可交互；加载 = 暂时不可交互，a11y 的
//!   `busy` 区分二者）；`selected` 映射到 `Active`（旧交互元素的选中态就是这个画法）；
//! - 状态切换用 `Transient<f32>` 的 `mix` 做 150 ms 过渡（旧 `ANIMATION_TICK`）：`bind_style` 在
//!   `previous` 与 `current` 两个状态的配方之间按 `mix` 插值，**不读**颜色瞬态 —— 主题切换时两端
//!   颜色一起重算，动画期间也不会停在旧主题的颜色上。
//!
//! # 指针按下
//!
//! 引擎只在抬起时派发 click（评估 G3），`App` 也没有公开的「按键是否按下」查询。按下态因此由
//! [`Interactive::dispatch`] 从外壳转发的事件流里取：外壳像对待 [`crate::kit::input::InputNode`]
//! 一样，把 `App::drain_events` 的事件逐条交给每个组件；按下时指针必须悬停在本节点子树内
//! （`App::hovered`），抬起 / 取消 / 离开一律清除。**键盘激活不需要本模块**：焦点节点的
//! Enter/Space 由引擎按 `NodeBuilder::on_click` 的声明分发。
//!
//! # 焦点
//!
//! 焦点环走动态 `StylePatch.border`（2 px `LineFocus`），而不是 `NodeBuilder::focus_ring`：引擎的
//! focus ring 颜色是挂载期常量，主题切换时不会更新；`bind_style` 每次求值都读当前外观，行为与
//! 其它动态色一致（登记见 `spec-deltas/kit-controls.md` §4）。
//!
//! # tooltip
//!
//! tooltip 组件在 P2-4b 建立。本模块只保留挂点：`tooltip_text` 存
//! 一段文本、`tooltip()` 读出，由 P2-4b 的 tooltip 层按节点几何挂载；本批不画任何浮层。
//!
//! # 生命周期
//!
//! 组件自有 `SignalScope`（[`Interactive::release`] 回收）与一个可选的 Action handler（回调注册
//! 时才有）。动态创建 / 销毁的控件在卸载节点后调用一次 `release`；重复调用幂等。
//!
//! # 与旧 API 的形态差异
//!
//! 旧 `Interactive` 是 UI 元素（`Styled` / `ParentElement` / `InteractiveElement`），新组件是
//! **挂载期 builder**：`new(app, theme, key)` 建状态，`build(app, window, parent, content)` 建节点
//! 并注册回调（content 拿 `&mut App` 是因为图标节点要写引擎资源缓存），`sync(app)` 帧驱动镜像，
//! `dispatch(app, event)` 转发指针事件。没有 `Styled` / `refine_style`（D24）。

use std::{cell::RefCell, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, InputEvent, PointerButton},
    interaction::{ActionCtx, AnimatedValue, AnimationSpec, Easing},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
    },
    paint::{
        color::{Color, ColorResolver},
        primitive::Border,
        style::{Cursor, StylePatch},
    },
    window::WindowId,
};

use crate::{
    kit::size::{UnifiedSizable, UnifiedSize},
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{
        components::{
            ComponentColors, InteractiveElementColors, InteractiveState, InteractiveStateColors,
            InteractiveVariant,
        },
        handle::ThemeHandle,
        role::Role,
    },
};

/// 圆角（旧 `rounded_sm` 的 4 px；主题 token 暂无圆角档位）。
const CORNER_RADIUS: f32 = 4.0;

/// 默认动作 key 的后缀（没有显式名字的 `on_click` 用它生成身份）。
const CLICK_ACTION_SUFFIX: &str = ".click";

/// 一种状态的三块颜色来源（`None` = 透明；角色在 `bind_style` 里按当前外观解析）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateRoles {
    /// 背景角色（`None` = 透明）。
    pub background: Option<Role>,
    /// 前景角色。
    pub foreground: Role,
    /// 边框角色（`None` = 透明）。
    pub border: Option<Role>,
}

/// 一个变体的四状态角色配方。
///
/// 主题的 `components.interactive` 只覆盖四种通用变体；`danger` 按钮与 tab 选中态这类
/// **控件特有**的画法用本类型表达：颜色仍是主题角色（随外观重算），只是不落在通用配方里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RolePalette {
    /// 未接触。
    pub normal: StateRoles,
    /// 指针所在。
    pub hover: StateRoles,
    /// 按下 / 选中。
    pub active: StateRoles,
    /// 禁用 / 加载。
    pub disabled: StateRoles,
}

/// 通用交互状态机（`Rc` 浅拷贝；组件持有它调 `sync` / `dispatch`）。
#[derive(Clone)]
pub struct Interactive {
    inner: Rc<InteractiveInner>,
    variant: InteractiveVariant,
    palette: Option<RolePalette>,
    size: UnifiedSize,
    disabled: bool,
    loading: bool,
    selected: bool,
    force_hover: bool,
    label: Option<SmolStr>,
    /// 动态可访问名（信号；优先于 `label`）。
    a11y_label_signal: Option<Signal<SmolStr>>,
    /// 展开态来源（`aria-expanded`）。
    a11y_expanded: Option<Signal<bool>>,
    a11y_role: A11yRole,
    checked: Option<Signal<bool>>,
    a11y_set: Option<(usize, usize)>,
    tooltip: Option<SmolStr>,
    action: Option<Action>,
    callback: Option<Rc<dyn Fn(&mut ActionCtx<'_>)>>,
    corner_radii: [f32; 4],
    tab_stop: bool,
    chrome: bool,
}

struct InteractiveInner {
    key: SmolStr,
    theme: ThemeHandle,
    scope: SignalScope,
    hovered: Signal<bool>,
    pressed: Signal<bool>,
    focused: Signal<bool>,
    disabled: Signal<bool>,
    loading: Signal<bool>,
    selected: Signal<bool>,
    force_hover: Signal<bool>,
    /// 当前视觉状态（`Normal` / `Hover` / `Active` / `Disabled`）。
    state: Signal<InteractiveState>,
    /// 上一个视觉状态（`mix` 插值的起点）。
    previous: Signal<InteractiveState>,
    /// 状态过渡进度（0 = 上一个状态，1 = 当前状态）。
    mix: Transient<f32>,
    /// 节点几何（`build` 之后可用）。
    geometry: RefCell<Geometry>,
    /// 本组件注册过的 Action（`release` 摘除；没有回调时是 `None`）。
    registered: RefCell<Option<Action>>,
    /// 需要随状态过渡一起重绘的子节点（文本 / 图标 / 几何；`bind_style` 读瞬态值的节点必须登记，
    /// 因为瞬态值不是依赖、引擎只重绘显式注册的节点集合）。
    watched: RefCell<Vec<NodeId>>,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    node: Option<NodeId>,
}

impl Interactive {
    /// 建状态机。`key` 是动作身份前缀与调试 id 前缀（同一 App 内唯一）。
    pub fn new(app: &mut App, theme: ThemeHandle, key: impl Into<SmolStr>) -> Self {
        let scope = app.signal_scope();
        Self {
            inner: Rc::new(InteractiveInner {
                key: key.into(),
                theme,
                scope,
                hovered: app.signal_scoped(scope, false),
                pressed: app.signal_scoped(scope, false),
                focused: app.signal_scoped(scope, false),
                disabled: app.signal_scoped(scope, false),
                loading: app.signal_scoped(scope, false),
                selected: app.signal_scoped(scope, false),
                force_hover: app.signal_scoped(scope, false),
                state: app.signal_scoped(scope, InteractiveState::Normal),
                previous: app.signal_scoped(scope, InteractiveState::Normal),
                mix: app.transient_scoped(scope, 1.0),
                geometry: RefCell::new(Geometry::default()),
                registered: RefCell::new(None),
                watched: RefCell::new(Vec::new()),
            }),
            variant: InteractiveVariant::default(),
            palette: None,
            size: UnifiedSize::Medium,
            disabled: false,
            loading: false,
            selected: false,
            force_hover: false,
            label: None,
            a11y_label_signal: None,
            a11y_expanded: None,
            a11y_role: A11yRole::Button,
            checked: None,
            a11y_set: None,
            tooltip: None,
            action: None,
            callback: None,
            corner_radii: [CORNER_RADIUS; 4],
            tab_stop: true,
            chrome: true,
        }
    }

    /// 变体（填充 / 控件色 / 描边 / 静默）。
    #[must_use]
    pub fn variant(mut self, variant: InteractiveVariant) -> Self {
        self.variant = variant;
        self
    }

    /// 用主题角色写的四状态配方（覆盖 `variant` 的通用配方；见 [`RolePalette`]）。
    #[must_use]
    pub fn role_palette(mut self, palette: RolePalette) -> Self {
        self.palette = Some(palette);
        self
    }

    /// 禁用（不可交互、`Disabled` 配方、`cursor: NotAllowed`）。
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// 加载态（不可交互 + `Disabled` 配方；a11y `busy`）。
    #[must_use]
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    /// 选中态（画成 `Active`；a11y `selected`）。
    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// 强制画成悬停（弹层盖住控件时的旧 `force_hover` 语义）。
    #[must_use]
    pub fn force_hover(mut self, force_hover: bool) -> Self {
        self.force_hover = force_hover;
        self
    }

    /// 可访问名（a11y `label`；组件通常把可见文字传进来）。
    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// a11y 角色（默认 `Button`；tab / toggle 各自覆盖）。
    #[must_use]
    pub fn a11y_role(mut self, role: A11yRole) -> Self {
        self.a11y_role = role;
        self
    }

    /// a11y 勾选态来源（`CheckBox` / `RadioButton` / `Switch` 用；写进 `A11yPatch.checked`）。
    #[must_use]
    pub fn checked_signal(mut self, checked: Signal<bool>) -> Self {
        self.checked = Some(checked);
        self
    }

    /// a11y 集合语义（`pos_in_set` / `size_of_set`；tab / 列表项用）。
    #[must_use]
    pub fn a11y_set(mut self, index: usize, count: usize) -> Self {
        self.a11y_set = Some((index, count));
        self
    }

    /// 动态可访问名来源（信号变化即 a11y 失效；优先于 [`Interactive::label`] 的静态值）。
    ///
    /// 下拉 / 选择的触发器用：可访问名 = 当前选中项的文本（`None` 时是占位文本），不能是构造期
    /// 固定的静态字符串。
    #[must_use]
    pub fn a11y_label_signal(mut self, label: Signal<SmolStr>) -> Self {
        self.a11y_label_signal = Some(label);
        self
    }

    /// 展开态来源（写进 `A11yPatch.expanded`，即 `aria-expanded`）。
    #[must_use]
    pub fn a11y_expanded(mut self, expanded: Signal<bool>) -> Self {
        self.a11y_expanded = Some(expanded);
        self
    }

    /// 去掉控件外壳的**绘制**（背景 / 边框）：toggle 这类「状态机在根、视觉在子节点」的组合控件用。
    /// 尺寸阶梯的高度 / 水平内边距仍然写进布局（旧 `unified_sized` 的行密度语义），前景 / 光标 /
    /// 命中开关照常输出。
    #[must_use]
    pub fn chrome(mut self, chrome: bool) -> Self {
        self.chrome = chrome;
        self
    }

    /// tooltip 文本挂点（P2-4b 的 tooltip 层读取；本批不画）。
    #[must_use]
    pub fn tooltip_text(mut self, text: impl Into<SmolStr>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// 显式动作身份（不注册回调时由调用方自己 `App::on_action`）。
    #[must_use]
    pub fn action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    /// 点击 / Enter / Space 激活的回调。`build` 时为它注册一个 Action handler；禁用或加载中的
    /// 程序化派发也会被回调守卫拦下（指针路径另由 `StylePatch.interactive = false` 拦）。
    #[must_use]
    pub fn on_click(mut self, callback: impl Fn(&mut ActionCtx<'_>) + 'static) -> Self {
        self.callback = Some(Rc::new(callback));
        self
    }

    /// 四角圆角（拼接控件用；默认 4 px）。
    #[must_use]
    pub fn corner_radius(mut self, radius: f32) -> Self {
        self.corner_radii = [radius; 4];
        self
    }

    /// 四角各自圆角（CSS 顺序 `[tl, tr, br, bl]`；拼接控件用 [`crate::kit::join`] 的结果）。
    #[must_use]
    pub fn corner_radii(mut self, radii: [f32; 4]) -> Self {
        self.corner_radii = radii;
        self
    }

    /// 是否进入 Tab 焦点序（默认 `true`）。
    #[must_use]
    pub fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }

    /// 尺寸档（子节点文本 / 图标取字号与图标边长用）。
    #[must_use]
    pub fn unified_size(&self) -> UnifiedSize {
        self.size
    }

    /// 动作身份（显式给的，或由回调自动生成的）。`build` 之前也返回自动生成的身份，
    /// 供组合控件 / 测试在挂载后按同一 key 派发。
    #[must_use]
    pub fn action_identity(&self) -> Option<Action> {
        self.action.clone().or_else(|| {
            self.callback
                .as_ref()
                .map(|_| Action::Custom(format_smolstr!("{}{CLICK_ACTION_SUFFIX}", self.inner.key)))
        })
    }

    /// tooltip 文本（P2-4b 挂点）。
    #[must_use]
    pub fn tooltip(&self) -> Option<&str> {
        self.tooltip.as_deref()
    }

    /// 动作 key / 调试 id 前缀。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 组件节点（`build` 之后可用）。
    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().node
    }

    /// 组件所属窗口（`build` 之后可用）。
    #[must_use]
    pub fn window(&self) -> Option<WindowId> {
        self.inner.geometry.borrow().window
    }

    /// 视觉状态（测试 / 组合控件用）。
    #[must_use]
    pub fn state(&self, app: &App) -> InteractiveState {
        app.read_with(&self.inner.state, |state| *state)
    }

    /// 是否悬停（帧同步后的镜像）。
    #[must_use]
    pub fn hovered(&self, app: &App) -> bool {
        app.read_with(&self.inner.hovered, |hovered| *hovered)
    }

    /// 是否按下（`dispatch` 转发按下 / 抬起事件后的镜像）。
    #[must_use]
    pub fn pressed(&self, app: &App) -> bool {
        app.read_with(&self.inner.pressed, |pressed| *pressed)
    }

    /// 是否聚焦。
    #[must_use]
    pub fn focused(&self, app: &App) -> bool {
        app.read_with(&self.inner.focused, |focused| *focused)
    }

    /// 状态过渡进度（0 = 上一状态，1 = 当前状态）。
    #[must_use]
    pub fn mix(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.mix)
    }

    /// 禁用态信号（组合控件的子节点要在自己的 `bind_style` 里读它，例如 toggle 的几何颜色）。
    #[must_use]
    pub fn disabled_signal(&self) -> Signal<bool> {
        self.inner.disabled
    }

    /// 运行时禁用。写信号 → 下次 `bind_style` 求值重画。
    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        app.write(&self.inner.disabled, disabled)
    }

    /// 运行时加载态。
    pub fn set_loading(&self, app: &mut App, loading: bool) -> bool {
        app.write(&self.inner.loading, loading)
    }

    /// 运行时选中态。
    pub fn set_selected(&self, app: &mut App, selected: bool) -> bool {
        app.write(&self.inner.selected, selected)
    }

    /// 运行时强制悬停。
    pub fn set_force_hover(&self, app: &mut App, force_hover: bool) -> bool {
        app.write(&self.inner.force_hover, force_hover)
    }

    /// 建根节点并挂 `content`（子图标 / 文本由调用方建；`content` 需要 `App` 是因为图标要走
    /// 引擎资源缓存）。
    ///
    /// 语义：
    /// - 布局：flex row + `items_center` + 尺寸阶梯的高度 / 间隙 / 水平内边距 + 1 px **布局**边框
    ///   （旧 `.border_1()` 的占位语义，`StyleBuilder::border`）；`chrome(false)` 去掉绘制外壳，
    ///   但高度 / 水平内边距仍写（旧 `unified_sized`）；圆角走静态 `PaintSpec`；
    /// - 动态：`bind_style` 输出背景 / 前景 / 边框 / 光标 / 命中开关；`bind_a11y` 输出
    ///   `disabled` / `busy` / `selected`；
    /// - 动作：显式 `action`，或由 `on_click` 自动生成 `"<key>.click"`；回调在 `build` 里注册，
    ///   并带禁用 / 加载守卫；
    /// - 焦点：`tab_stop` 与动态焦点边框。
    ///
    /// # Errors
    /// [`EngineError::UnknownWindow`]（窗口已关闭）/ `StaleNode`（父节点陈旧）/ 引擎建树错误。
    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        content: impl FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>,
    ) -> Result<NodeId, EngineError> {
        if app.window(window).is_none() {
            return Err(EngineError::UnknownWindow { window });
        }
        let inner = &self.inner;
        // 初始值：build 之前写（尚无依赖者，等值短路不产生帧）。
        app.write(&inner.disabled, self.disabled);
        app.write(&inner.loading, self.loading);
        app.write(&inner.selected, self.selected);
        app.write(&inner.force_hover, self.force_hover);
        let initial = initial_state(self.disabled, self.loading, self.selected);
        app.write(&inner.state, initial);
        app.write(&inner.previous, initial);
        app.write_transient(&inner.mix, 1.0);

        let action = self.action.clone().or_else(|| {
            self.callback
                .as_ref()
                .map(|_| Action::Custom(format_smolstr!("{}{CLICK_ACTION_SUFFIX}", inner.key)))
        });
        if let (Some(callback), Some(action)) = (self.callback.clone(), action.clone()) {
            let disabled = inner.disabled;
            let loading = inner.loading;
            app.on_action(action.clone(), move |ctx| {
                let off = *ctx.read(&disabled) || *ctx.read(&loading);
                if off {
                    return;
                }
                callback(ctx);
            });
            *inner.registered.borrow_mut() = Some(action);
        }

        let theme = inner.theme.clone();
        let variant = self.variant;
        let palette = self.palette;
        let state = inner.state;
        let previous = inner.previous;
        let mix = inner.mix;
        let disabled = inner.disabled;
        let loading = inner.loading;
        let selected = inner.selected;
        let focused = inner.focused;
        let a11y_label = self.label.clone();
        let a11y_label_signal = self.a11y_label_signal;
        let a11y_expanded = self.a11y_expanded;
        let a11y_role = self.a11y_role;
        let checked = self.checked;
        let a11y_set = self.a11y_set;
        let chrome = self.chrome;

        let style = if chrome {
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(self.size.gap())
                .padding_x(self.size.px())
                .height(self.size.h())
                .border(theme.tokens().border_width)
                .build()
        } else {
            // 旧 `unified_sized` 的布局仍要写：toggle 这类「状态机在根、视觉在子节点」的组合控件
            // 根上没有外壳，但行高与左侧留白是它布局的一部分（旧 toggle 根 = h 30 / gap 6 / px 6）。
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(self.size.gap())
                .height(self.size.h())
                .padding_x(self.size.px())
                .build()
        };
        let paint = {
            let mut paint = PaintBuilder::new().build();
            paint.corner_radius = self.corner_radii;
            paint
        };
        let base_border = theme.tokens().border_width;
        let focus_border = theme.tokens().border_width * 2.0;

        let mut builder = NodeBuilder::boxed()
            .id(format_smolstr!("{}:interactive", inner.key))
            .style(style)
            .paint(paint)
            .tab_stop(self.tab_stop)
            .bind_style(move |cx| {
                let (current, last, progress) = {
                    let current = *cx.read(&state);
                    let last = *cx.read(&previous);
                    let progress = *cx.read_transient(&mix);
                    (current, last, progress)
                };
                let colors = lerped_colors(cx, &theme, variant, palette, last, current, progress);
                let focus = *cx.read(&focused);
                let focus_color = theme.role(cx, Role::LineFocus);
                let enabled = !*cx.read(&disabled) && !*cx.read(&loading);
                StylePatch {
                    background: chrome.then_some(colors.background),
                    foreground: Some(colors.foreground),
                    border: chrome.then_some(Border {
                        width: if focus { focus_border } else { base_border },
                        color: if focus { focus_color } else { colors.border },
                    }),
                    interactive: Some(enabled),
                    cursor: Some(if enabled {
                        Cursor::Pointer
                    } else {
                        Cursor::NotAllowed
                    }),
                    ..StylePatch::default()
                }
            })
            .bind_a11y(move |cx| A11yPatch {
                label: match a11y_label_signal {
                    Some(signal) => Some(cx.read(&signal).clone()),
                    None => a11y_label.clone(),
                },
                expanded: a11y_expanded.map(|signal| *cx.read(&signal)),
                disabled: Some(*cx.read(&disabled) || *cx.read(&loading)),
                busy: Some(*cx.read(&loading)),
                selected: Some(*cx.read(&selected)),
                checked: checked.map(|signal| *cx.read(&signal)),
                pos_in_set: a11y_set.map(|(index, _)| index),
                size_of_set: a11y_set.map(|(_, count)| count),
                ..A11yPatch::default()
            })
            .a11y(A11yOverride {
                role: Some(a11y_role),
                label: self.label.clone(),
                ..A11yOverride::default()
            });
        if let Some(action) = action {
            builder = builder.on_click(action);
        }
        let node = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            builder.build(tree, parent)?
        };
        {
            let mut geometry = inner.geometry.borrow_mut();
            geometry.window = Some(window);
            geometry.node = Some(node);
        }
        content(app, window, node)?;
        Ok(node)
    }

    /// 建一个没有任何子节点的交互根（图标 / 文本由调用方之后自己挂）。
    ///
    /// # Errors
    /// 同 [`Interactive::build`]。
    pub fn build_bare(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        self.build(app, window, parent, |_, _, _| Ok(()))
    }

    /// 子节点的动态前景色（文本 / E7 图标共用：`StylePatch::foreground`）。
    ///
    /// 组件把它包进自己的 `bind_style`：`move |cx| StylePatch { foreground: Some(interactive
    /// .foreground_color(cx)), ..StylePatch::default() }`。
    pub fn foreground_color(&self, cx: &mut ReadCtx<'_>) -> Color {
        let (current, last, progress) = {
            let current = *cx.read(&self.inner.state);
            let last = *cx.read(&self.inner.previous);
            let progress = *cx.read_transient(&self.inner.mix);
            (current, last, progress)
        };
        lerped_colors(
            cx,
            &self.inner.theme,
            self.variant,
            self.palette,
            last,
            current,
            progress,
        )
        .foreground
    }

    /// 帧驱动同步：镜像悬停 / 焦点，并在状态变化时启动 150 ms 过渡。
    ///
    /// 返回是否写入了状态（`false` = 本帧无事发生，不请求帧 —— I3）。
    ///
    /// # Errors
    /// 动画注册失败（窗口已关闭）。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let (window, node) = {
            let geometry = self.inner.geometry.borrow();
            (geometry.window, geometry.node)
        };
        let (Some(window), Some(node)) = (window, node) else {
            return Ok(false);
        };
        let hovered = node_in_subtree(app, window, node, app.hovered(window));
        let focused = node_in_subtree(app, window, node, app.focused(window));
        let mut changed = app.write(&self.inner.hovered, hovered);
        changed |= app.write(&self.inner.focused, focused);

        let disabled = app.read_with(&self.inner.disabled, |value| *value);
        let loading = app.read_with(&self.inner.loading, |value| *value);
        let pressed = app.read_with(&self.inner.pressed, |value| *value);
        let selected = app.read_with(&self.inner.selected, |value| *value);
        let force_hover = app.read_with(&self.inner.force_hover, |value| *value);
        let next = if disabled || loading {
            InteractiveState::Disabled
        } else if pressed || selected {
            InteractiveState::Active
        } else if hovered || force_hover {
            InteractiveState::Hover
        } else {
            InteractiveState::Normal
        };
        let last = app.read_with(&self.inner.state, |state| *state);
        if next != last {
            app.write(&self.inner.previous, last);
            app.write(&self.inner.state, next);
            app.write_transient(&self.inner.mix, 0.0);
            let nodes = self.animation_nodes();
            app.animate(
                window,
                AnimatedValue::Scalar {
                    target: self.inner.mix,
                    to: 1.0,
                },
                &nodes,
                AnimationSpec {
                    duration: self.inner.theme.tokens().animation_tick,
                    easing: Easing::EaseOutCubic,
                },
            )?;
            changed = true;
        }
        Ok(changed)
    }

    /// 转发一个平台事件（外壳在 `App::drain_events` 之后调用）。
    ///
    /// 只消费主键的按下 / 抬起 / 取消 / 离开：按下时光标必须在本节点子树内；禁用 / 加载中不记录。
    /// 返回是否改了按下态。键盘激活由引擎分发 `on_click` 的动作，**不经过**这里。
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let (window, node) = {
            let geometry = self.inner.geometry.borrow();
            (geometry.window, geometry.node)
        };
        let (Some(window), Some(node)) = (window, node) else {
            return false;
        };
        let Event::Input(input) = event else {
            return false;
        };
        let enabled = !app.read_with(&self.inner.disabled, |value| *value)
            && !app.read_with(&self.inner.loading, |value| *value);
        match input {
            InputEvent::PointerDown {
                button: PointerButton::Primary,
                ..
            } => {
                if !enabled {
                    return false;
                }
                if !node_in_subtree(app, window, node, app.hovered(window)) {
                    return false;
                }
                app.write(&self.inner.pressed, true)
            }
            InputEvent::PointerUp {
                button: PointerButton::Primary,
                ..
            }
            | InputEvent::PointerCancel { .. }
            | InputEvent::PointerLeave { .. } => app.write(&self.inner.pressed, false),
            _ => false,
        }
    }

    /// 登记一个「随状态过渡重绘」的子节点（文本 / 图标 / 几何节点）。
    ///
    /// `bind_style` 读 `Transient` 不产生依赖边，动画只给**显式注册的节点**标脏；所以子节点上
    /// 读 `foreground_color` / `mix` 的 `bind_style` 必须在这里登记，否则过渡期间会停在旧值。
    pub fn watch(&self, node: NodeId) {
        let mut watched = self.inner.watched.borrow_mut();
        if !watched.contains(&node) {
            watched.push(node);
        }
    }

    /// 本组件的动画目标节点（根 + 已登记的子树节点）。
    #[must_use]
    pub fn animation_nodes(&self) -> Vec<NodeId> {
        let mut nodes = Vec::with_capacity(1 + self.inner.watched.borrow().len());
        if let Some(node) = self.inner.geometry.borrow().node {
            nodes.push(node);
        }
        nodes.extend(self.inner.watched.borrow().iter().copied());
        nodes
    }

    /// 释放本组件的 Action handler 与信号作用域（动态创建 / 销毁的控件调用；先卸载节点）。
    ///
    /// 返回被释放的槽位数（9 个信号 / 瞬态槽）；重复调用返回 0（幂等）。释放后清掉几何句柄：
    /// `sync` 不再拿陈旧 `NodeId` 去查树。
    pub fn release(&self, app: &mut App) -> usize {
        if let Some(action) = self.inner.registered.borrow_mut().take() {
            app.off_action(&action);
        }
        *self.inner.geometry.borrow_mut() = Geometry::default();
        app.release_scope(self.inner.scope)
    }
}

impl UnifiedSizable for Interactive {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.size = size;
        self
    }
}

/// 初始视觉状态（`build` 时与信号一起写；`mix` 从 1 开始，不播放入场动画）。
fn initial_state(disabled: bool, loading: bool, selected: bool) -> InteractiveState {
    if disabled || loading {
        InteractiveState::Disabled
    } else if selected {
        InteractiveState::Active
    } else {
        InteractiveState::Normal
    }
}

/// 一个变体的四状态配方。
fn variant_colors(
    components: &ComponentColors,
    variant: InteractiveVariant,
) -> &InteractiveStateColors {
    match variant {
        InteractiveVariant::Filled => &components.interactive.filled,
        InteractiveVariant::Tonal => &components.interactive.tonal,
        InteractiveVariant::Outline => &components.interactive.outline,
        InteractiveVariant::Ghost => &components.interactive.ghost,
    }
}

/// 两个状态配方之间的线性预乘插值（与引擎渐变 / 动画同一插值空间）。
fn lerped_colors(
    cx: &mut ReadCtx<'_>,
    theme: &ThemeHandle,
    variant: InteractiveVariant,
    palette: Option<RolePalette>,
    from: InteractiveState,
    to: InteractiveState,
    mix: f32,
) -> InteractiveElementColors {
    let from = state_colors(cx, theme, variant, palette, from);
    let to = state_colors(cx, theme, variant, palette, to);
    let resolver = ColorResolver;
    InteractiveElementColors {
        background: resolver.lerp_color(from.background, to.background, mix),
        foreground: resolver.lerp_color(from.foreground, to.foreground, mix),
        border: resolver.lerp_color(from.border, to.border, mix),
    }
}

/// 一个状态的三块颜色（主题通用配方，或控件自带的 [`RolePalette`]）。
fn state_colors(
    cx: &mut ReadCtx<'_>,
    theme: &ThemeHandle,
    variant: InteractiveVariant,
    palette: Option<RolePalette>,
    state: InteractiveState,
) -> InteractiveElementColors {
    if let Some(palette) = palette {
        let roles = match state {
            InteractiveState::Normal => palette.normal,
            InteractiveState::Hover => palette.hover,
            InteractiveState::Active => palette.active,
            InteractiveState::Disabled => palette.disabled,
        };
        return InteractiveElementColors {
            background: roles
                .background
                .map_or(Color::TRANSPARENT, |role| theme.role(cx, role)),
            foreground: theme.role(cx, roles.foreground),
            border: roles
                .border
                .map_or(Color::TRANSPARENT, |role| theme.role(cx, role)),
        };
    }
    *state_entry(variant_colors(theme.components(cx), variant), state)
}

/// 状态 → 配方条目。
fn state_entry(
    colors: &InteractiveStateColors,
    state: InteractiveState,
) -> &InteractiveElementColors {
    match state {
        InteractiveState::Normal => &colors.normal,
        InteractiveState::Hover => &colors.hover,
        InteractiveState::Active => &colors.active,
        InteractiveState::Disabled => &colors.disabled,
    }
}

/// `candidate` 是否等于 `root` 或在其子树内（悬停 / 焦点镜像用）。
fn node_in_subtree(app: &App, window: WindowId, root: NodeId, candidate: Option<NodeId>) -> bool {
    let Some(target) = app.window(window) else {
        return false;
    };
    let tree = target.tree();
    let mut cursor = candidate;
    while let Some(node) = cursor {
        if node == root {
            return true;
        }
        cursor = tree.get(node).ok().and_then(|target| target.parent());
    }
    false
}

/// 挂一张可动态着色的 E7 图标（`StylePatch::foreground` 调制）。
///
/// 为什么不直接用 [`crate::kit::icon::IconSpec::build`]：它把颜色写死成挂载期
/// `image_modulation`，而按钮 / tab 的前景色随状态与外观变化。`IconSpec::resource_for` 是
/// `icon.rs` 的私有实现（本批写作用域外），这里按同一公开口径复刻 15 行：按窗口 DPI 选
/// [`RasterTier`]，以「逻辑尺寸 × scale」为物理目标取图集变体（D-10），窗口归属登记。节点本身与
/// `IconSpec` 一致：显式正方形 `Style` + `ImageFit::Fill` + 装饰性 a11y `hidden`。
pub(crate) fn mount_dynamic_icon(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    kind: crate::kit::icon::IconKind,
    size: f32,
    bind_style: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
) -> Result<NodeId, EngineError> {
    debug_assert!(
        size.is_finite() && size > 0.0,
        "icon size must be a positive logical length"
    );
    let scale = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .scale()
        .factor();
    let tier = crate::kit::icon::RasterTier::for_scale(scale);
    let bytes = crate::kit::icon::raster(kind, tier);
    let target = ui_core::math::Size::new(size * scale, size * scale);
    let resource = app
        .resources_mut()
        .insert_owned_at_size(window, bytes, target)?;
    let mut style = ui_core::paint::style::Style::default();
    style.inner.size.width = taffy::style::Dimension::length(size);
    style.inner.size.height = taffy::style::Dimension::length(size);
    NodeBuilder::image(resource.id, ui_core::node::image::ImageFit::Fill)
        .style(style)
        .bind_style(bind_style)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(
            app.window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut(),
            parent,
        )
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use taffy::style::Dimension;
    use ui_core::{
        a11y::A11yRole,
        event::{ActionSource, Event, InputEvent, KeyCode, Modifiers, PointerButton},
        math::Point,
        node::{dirty::DirtyFlags, id::NodeId},
        paint::{
            color::{Color, ColorResolver},
            style::Cursor,
        },
        test_support::app::TestApp,
    };

    use super::{Interactive, InteractiveState, InteractiveVariant};
    use crate::{
        kit::size::{UnifiedSizable, UnifiedSize},
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme},
    };

    struct Harness {
        app: TestApp,
        interactive: Interactive,
        node: NodeId,
    }

    fn harness_with(configure: impl FnOnce(Interactive) -> Interactive) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let interactive = configure(Interactive::new(app.app_mut(), theme, "test"));
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = interactive
            .build_bare(app.app_mut(), window, root)
            .expect("build");
        let _ = app.drain().expect("首帧");
        Harness {
            app,
            interactive,
            node,
        }
    }

    fn harness() -> Harness {
        harness_with(std::convert::identity)
    }

    /// 让状态过渡动画走完，并刷新最后一帧的样式补丁。
    fn settle(harness: &mut Harness) {
        for _ in 0..4 {
            harness.app.advance(Duration::from_millis(60));
            let _ = harness.app.drain().expect("frame");
            let _ = harness.interactive.sync(harness.app.app_mut());
        }
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.interactive.sync(harness.app.app_mut());
        let _ = harness.app.drain().expect("frame");
    }

    fn center(app: &TestApp, node: NodeId) -> Point {
        let bounds = app.probe().bounds(node).expect("bounds");
        Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn hover(harness: &mut Harness) {
        let point = center(&harness.app, harness.node);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(point, Modifiers::empty()));
        let _ = harness.interactive.sync(harness.app.app_mut());
        settle(harness);
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

    fn light_theme() -> Theme {
        Theme::of(Appearance::Light)
    }

    /// 颜色的颜色空间可能不同（主题角色是 OKLCH、渲染补丁是线性预乘），按引擎解析后的
    /// 线性预乘分量比较 —— 这也是「看起来一样」的唯一口径。
    fn resolved(color: Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    fn assert_color(actual: Option<Color>, expected: Color, message: &str) {
        assert_eq!(actual.map(resolved), Some(resolved(expected)), "{message}");
    }

    /// 悬停：`sync` 把 `App::hovered` 镜像成 `Hover`，`bind_style` 输出悬停配方。
    #[test]
    fn hovering_switches_the_state_and_the_patch_to_the_hover_recipe() {
        let mut harness = harness();
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Normal
        );
        hover(&mut harness);
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Hover,
            "指针悬停后状态必须是 Hover"
        );
        assert!(harness.interactive.hovered(harness.app.app()));
        let recipe = light_theme().components().interactive.filled.hover;
        assert_color(patch(&harness).background, recipe.background, "背景配方");
        assert_color(patch(&harness).foreground, recipe.foreground, "前景配方");

        let point = Point::new(400.0, 300.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(point, Modifiers::empty()));
        let _ = harness.interactive.sync(harness.app.app_mut());
        settle(&mut harness);
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Normal,
            "指针离开后回到 Normal"
        );
    }

    /// 按下态：`dispatch` 收到主键按下 → `Active`；抬起 → 回到悬停态。
    #[test]
    fn pressing_and_releasing_the_primary_button_moves_through_active() {
        let mut harness = harness();
        hover(&mut harness);
        let point = center(&harness.app, harness.node);
        let down = Event::Input(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert!(
            harness.interactive.dispatch(harness.app.app_mut(), &down),
            "按下必须更新按下信号"
        );
        assert!(harness.interactive.pressed(harness.app.app()));
        let _ = harness.interactive.sync(harness.app.app_mut());
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Active
        );
        settle(&mut harness);
        let recipe = light_theme().components().interactive.filled.active;
        assert_color(patch(&harness).background, recipe.background, "背景配方");

        let up = Event::Input(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert!(harness.interactive.dispatch(harness.app.app_mut(), &up));
        assert!(!harness.interactive.pressed(harness.app.app()));
        let _ = harness.interactive.sync(harness.app.app_mut());
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Hover,
            "抬起后回到悬停（指针仍在控件上）"
        );
    }

    /// 禁用：`Disabled` 配方 + 不可交互 + `NotAllowed`；按下事件被忽略。
    #[test]
    fn a_disabled_control_paints_the_disabled_recipe_and_ignores_the_pointer() {
        let mut harness = harness_with(|interactive| interactive.disabled(true));
        hover(&mut harness);
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Disabled
        );
        let recipe = light_theme().components().interactive.filled.disabled;
        assert_color(patch(&harness).background, recipe.background, "背景配方");
        assert_eq!(patch(&harness).interactive, Some(false));
        assert_eq!(patch(&harness).cursor, Some(Cursor::NotAllowed));

        let point = center(&harness.app, harness.node);
        let down = Event::Input(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert!(
            !harness.interactive.dispatch(harness.app.app_mut(), &down),
            "禁用控件不得进入按下态（变异：去掉这里的守卫）"
        );
        assert!(!harness.interactive.pressed(harness.app.app()));
    }

    /// 加载：与禁用同配方，但 a11y `busy` 为真（语义区分）。
    #[test]
    fn loading_paints_disabled_but_reports_busy_to_a11y() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let interactive = Interactive::new(app.app_mut(), theme, "loading").loading(true);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = interactive
            .build_bare(app.app_mut(), window, root)
            .expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("节点进 a11y");
        assert_eq!(record.role, A11yRole::Button);
        assert!(record.busy, "加载态必须宣告 busy");
    }

    /// 焦点：`set_focus` + `sync` 后 `bind_style` 画出 2 px `LineFocus` 边框。
    #[test]
    fn a_focused_control_draws_the_focus_border() {
        let mut harness = harness();
        let window = harness.app.window_id();
        assert!(harness.app.app_mut().set_focus(window, Some(harness.node)));
        let _ = harness.interactive.sync(harness.app.app_mut());
        settle(&mut harness);
        assert!(harness.interactive.focused(harness.app.app()));
        let border = patch(&harness).border.expect("焦点边框");
        assert_eq!(border.width, 2.0, "焦点环是 2 px");
        assert_eq!(
            resolved(border.color),
            resolved(light_theme().role(crate::theme::role::Role::LineFocus))
        );
        assert!(harness.app.app_mut().set_focus(window, None));
        let _ = harness.interactive.sync(harness.app.app_mut());
        settle(&mut harness);
        assert!(!harness.interactive.focused(harness.app.app()));
        assert_eq!(patch(&harness).border.expect("普通边框").width, 1.0);
    }

    /// 键盘激活：焦点节点上的 Enter / Space 由引擎按 `on_click` 声明分发到同一个回调。
    #[test]
    fn enter_and_space_on_the_focused_control_invoke_the_callback() {
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let mut harness = harness_with(move |interactive| {
            interactive.on_click(move |_| counter.set(counter.get() + 1))
        });
        let window = harness.app.window_id();
        assert!(harness.app.app_mut().set_focus(window, Some(harness.node)));
        for code in [KeyCode::Enter, KeyCode::Space] {
            let _ = harness
                .app
                .inject(InputEvent::key_down(code, Modifiers::empty()));
            let _ = harness
                .app
                .inject(InputEvent::key_up(code, Modifiers::empty()));
        }
        assert_eq!(calls.get(), 2, "Enter / Space 各激活一次");
    }

    /// 鼠标点击：按下 + 抬起（指针路径）触发同一个回调。
    #[test]
    fn a_click_invokes_the_callback_and_a_press_does_not() {
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let mut harness = harness_with(move |interactive| {
            interactive.on_click(move |_| counter.set(counter.get() + 1))
        });
        let point = center(&harness.app, harness.node);
        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert_eq!(calls.get(), 0, "只有抬起才派发 click");
        let _ = harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert_eq!(calls.get(), 1);
    }

    /// 禁用 / 加载中的程序化派发也被回调守卫拦下（引擎的 `interactive=false` 只拦指针路径）。
    #[test]
    fn programmatic_dispatch_cannot_click_a_disabled_control() {
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let mut harness = harness_with(move |interactive| {
            interactive
                .disabled(true)
                .on_click(move |_| counter.set(counter.get() + 1))
        });
        let window = harness.app.window_id();
        let action = harness
            .interactive
            .action_identity()
            .expect("自动生成的动作身份");
        let node = harness.node;
        let _ =
            harness
                .app
                .app_mut()
                .dispatch_action(window, node, action, ActionSource::Programmatic);
        assert_eq!(calls.get(), 0, "禁用按钮的程序化 click 不得触发 on_click");
    }

    /// 四种变体 × 四种状态：`bind_style` 的补丁与主题配方逐色相等。
    #[test]
    fn every_variant_and_state_matches_the_theme_recipe() {
        let cases = [
            (InteractiveVariant::Filled, "filled"),
            (InteractiveVariant::Tonal, "tonal"),
            (InteractiveVariant::Outline, "outline"),
            (InteractiveVariant::Ghost, "ghost"),
        ];
        for (variant, name) in cases {
            let states = [
                (InteractiveState::Normal, "normal", false, false),
                (InteractiveState::Hover, "hover", false, false),
                (InteractiveState::Active, "active", false, true),
                (InteractiveState::Disabled, "disabled", true, false),
            ];
            for (expected, state_name, disabled, selected) in states {
                let mut harness = harness_with(|interactive| {
                    interactive
                        .variant(variant)
                        .selected(selected)
                        .disabled(disabled)
                });
                if expected == InteractiveState::Hover {
                    hover(&mut harness);
                }
                assert_eq!(
                    harness.interactive.state(harness.app.app()),
                    expected,
                    "{name}/{state_name} 的状态"
                );
                let theme = light_theme();
                let colors = super::variant_colors(theme.components(), variant);
                let expected_colors = super::state_entry(colors, expected);
                assert_color(
                    patch(&harness).background,
                    expected_colors.background,
                    &format!("{name}/{state_name} 的背景"),
                );
                assert_color(
                    patch(&harness).foreground,
                    expected_colors.foreground,
                    &format!("{name}/{state_name} 的前景"),
                );
            }
        }
    }

    /// I3：稳态 `sync` 不写状态、不出帧。
    #[test]
    fn a_settled_control_produces_no_further_frames() {
        let mut harness = harness();
        hover(&mut harness);
        let _ = harness.app.drain().expect("frame");
        assert!(
            !harness
                .interactive
                .sync(harness.app.app_mut())
                .expect("sync"),
            "稳态 sync 必须返回 false"
        );
        let record = harness.app.drain().expect("idle frame").clone();
        assert!(!record.rendered, "稳态不得出帧（I3）");
    }

    /// 运行时 `set_disabled` / `set_loading` 只写一次信号；之后 `sync` 走状态迁移。
    #[test]
    fn runtime_flags_drive_the_state_machine() {
        let mut harness = harness();
        assert!(harness.interactive.set_loading(harness.app.app_mut(), true));
        let _ = harness.interactive.sync(harness.app.app_mut());
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Disabled
        );
        assert!(
            harness
                .interactive
                .set_loading(harness.app.app_mut(), false)
        );
        let _ = harness.interactive.sync(harness.app.app_mut());
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Normal
        );
        assert!(
            !harness
                .interactive
                .set_loading(harness.app.app_mut(), false),
            "等值写入短路"
        );
    }

    /// `force_hover`：弹层盖住控件时不动指针也画成悬停；清除后回到普通态。
    #[test]
    fn force_hover_paints_the_hover_recipe_without_the_pointer() {
        let mut harness = harness_with(|interactive| interactive.force_hover(true));
        let _ = harness.interactive.sync(harness.app.app_mut());
        settle(&mut harness);
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Hover,
            "force_hover 必须让状态机停在 Hover"
        );
        let recipe = light_theme().components().interactive.filled.hover;
        assert_color(
            patch(&harness).background,
            recipe.background,
            "强制悬停背景",
        );
        assert!(
            harness
                .interactive
                .set_force_hover(harness.app.app_mut(), false)
        );
        let _ = harness.interactive.sync(harness.app.app_mut());
        settle(&mut harness);
        assert_eq!(
            harness.interactive.state(harness.app.app()),
            InteractiveState::Normal,
            "清除 force_hover 后回到 Normal"
        );
    }

    /// tooltip 挂点：文本在 `build` 前后都可读（P2-4b 的 tooltip 层按它挂浮层）。
    #[test]
    fn the_tooltip_text_is_kept_for_the_tooltip_layer() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let interactive = Interactive::new(app.app_mut(), theme, "tooltip").tooltip_text("Reload");
        assert_eq!(interactive.tooltip(), Some("Reload"));
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = interactive
            .build_bare(app.app_mut(), window, root)
            .expect("build");
        let _ = app.drain().expect("frame");
        assert_eq!(interactive.tooltip(), Some("Reload"));
        assert_eq!(interactive.node(), Some(node));
    }

    /// 失效通道：悬停只重算动态属性 + 重绘，**不**标 `LAYOUT`
    /// （变异：`StylePatch` 加 `affects_layout: true` → 本测试红）。
    #[test]
    fn hovering_repaints_without_marking_layout() {
        let mut harness = harness();
        let point = center(&harness.app, harness.node);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(point, Modifiers::empty()));
        let _ = harness.interactive.sync(harness.app.app_mut());
        let window = harness.app.window_id();
        harness
            .app
            .app_mut()
            .refresh_styles(window)
            .expect("style pass");
        let dirty = harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(harness.node)
            .expect("node")
            .dirty();
        assert!(
            dirty.contains(DirtyFlags::PAINT),
            "悬停必须请求重绘（实际 {dirty:?}）"
        );
        assert!(
            !dirty.contains(DirtyFlags::LAYOUT),
            "纯绘制属性不得触发布局（实际 {dirty:?}）"
        );
    }

    /// 状态过渡真的由引擎动画时钟推进（`mix` 从 0 到 1）。
    #[test]
    fn the_state_transition_is_animated_by_the_engine() {
        let mut harness = harness();
        let point = center(&harness.app, harness.node);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(point, Modifiers::empty()));
        let _ = harness.interactive.sync(harness.app.app_mut());
        let window = harness.app.window_id();
        assert!(
            harness.app.app().animations_running(window),
            "状态变化必须注册动画"
        );
        assert!(
            harness.interactive.mix(harness.app.app()) < 0.01,
            "过渡从 mix = 0 开始"
        );
        harness.app.advance(Duration::from_millis(300));
        let _ = harness.app.drain().expect("frame");
        assert!(
            (harness.interactive.mix(harness.app.app()) - 1.0).abs() < 1.0e-4,
            "过渡结束后 mix 必须到达 1（实际 {}）",
            harness.interactive.mix(harness.app.app())
        );
    }

    /// a11y：角色 / 标签 / 选中态都进快照（变异：删掉 `.a11y` 的 role → 本测试红）。
    #[test]
    fn the_control_declares_role_label_and_state_to_a11y() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let interactive = Interactive::new(app.app_mut(), theme, "a11y")
            .label("Save")
            .selected(true);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = interactive
            .build_bare(app.app_mut(), window, root)
            .expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("节点进 a11y");
        assert_eq!(
            record.role,
            A11yRole::Button,
            "交互控件默认 a11y 角色是 Button（变异：删掉角色声明 → 红）"
        );
        assert_eq!(record.label_text(), Some("Save"));
        assert_eq!(record.selected, Some(true));
    }

    /// 停用：信号作用域回收，重复调用幂等。
    #[test]
    fn releasing_the_control_reclaims_its_slots() {
        let mut harness = harness();
        let first = harness.interactive.release(harness.app.app_mut());
        assert!(first >= 9, "9 个信号 / 瞬态槽都该被回收（实际 {first}）");
        assert_eq!(harness.interactive.release(harness.app.app_mut()), 0);
    }

    /// 尺寸阶梯写进布局样式（高度 / 间隙 / 水平内边距），布局盒首帧可读。
    #[test]
    fn the_size_ladder_is_written_into_the_layout_style() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let interactive =
            Interactive::new(app.app_mut(), theme, "size").with_unified_size(UnifiedSize::Large);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = interactive
            .build_bare(app.app_mut(), window, root)
            .expect("build");
        let _ = app.drain().expect("frame");
        let tree = app.app().window(window).expect("window").tree();
        let layout = tree.get(node).expect("node").style();
        assert_eq!(
            layout.inner.size.height,
            Dimension::length(UnifiedSize::Large.h())
        );
        assert_eq!(
            layout.inner.padding.left,
            taffy::style::LengthPercentage::length(10.0)
        );
        let bounds = app.probe().bounds(node).expect("bounds");
        assert!(bounds.size.height >= UnifiedSize::Large.h() - 0.5);
    }
}
