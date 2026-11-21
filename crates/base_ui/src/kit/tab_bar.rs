//! 标签栏（旧 `crate::tab_bar` 的引擎化重建，P2-4a）。
//!
//! 旧实现依赖 gpui-base 的 `Tabs` / `Tab`：轨道（`surface.sunken`）里放标签，`Fade` 模式给每个
//! 标签做背景色过渡，`Slide` 模式用 `on_prepaint` 量每个标签的 bounds 再驱动一条 `spring` 指示条。
//! 新引擎没有布局后量测钩子，也没有 spring；本实现用**帧同步**取几何、用 `Transient<f32>` +
//! `App::animate` 做插值：
//!
//! ```text
//! build: 轨道(List) → 行(relative) → 指示条(LayoutOverride) + N 个标签(Interactive)
//! sync : 读选中 → 每个标签的 hover/focus/selected；Slide 模式按标签当前布局盒
//!        算指示条目标，目标变化时 animate(left/width/height)，每帧写 LayoutOverride
//! ```
//!
//! # 交互
//!
//! - 点击标签（或焦点标签上的 Enter/Space）选中它并调 `on_change(index)`；
//! - **键盘方向键**：`Left` / `Right` 用 E8 的 `App::bind_key_scoped` 绑在**行节点**上 —— 只有焦点
//!   落在本标签栏子树内时才消费，同屏多个标签栏 / 滚动容器的方向键互不干扰；移动后焦点跟到新标签；
//! - **关闭按钮**：`closable(true)` 且标签数 > 1 时每个标签右侧画一个 X（旧实现同样在只剩一个标签
//!   时隐藏关闭按钮）；点击只调 `on_close(index)`，不改变选中（调用方负责移除标签并重挂）；
//! - 溢出滚动：旧 `tab_bar` **没有**滚动（评估 §7.1：386 行里无滚动容器），本批同样不做。
//!
//! # 动画
//!
//! `Fade`：标签背景 / 文字色由 [`Interactive`] 的状态过渡（150 ms）；`Slide`：指示条位置 / 宽度由
//! 引擎动画时钟插值。两者都不建应用层帧循环，稳态不出帧（I3）。
//!
//! # 生命周期
//!
//! 标签 / 关闭按钮的 handler 持有 `Rc<TabBarInner>`（选中与关闭回调要能跨帧读配置），因此
//! [`TabBar::release`] 必须调用：它摘 scoped 键、Action handler、各 Interactive 的 scope，并清空
//! 标签表以打断 `Rc` 环。
//!
//! # a11y
//!
//! 引擎 `A11yRole` **没有 `Tab` / `TabList`**（§7.4 只有列表 / 表格 / 树；E9 也没有加）。本批用
//! `List`（轨道）+ `ListBoxOption`（标签）表达：标签声明可访问名、`pos_in_set` / `size_of_set` 与
//! `selected`。Tabs 模式的 AT 专有语义（方向键树内导航 / `aria-controls` 关系）登记为缺口，待引擎
//! 补角色（登记见 spec-delta §4）。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, KeyBinding, KeyCode, Modifiers},
    interaction::{ActionCtx, AnimatedValue, AnimationSpec, Easing},
    layout::LayoutOverride,
    math::Rect,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
    },
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        icon::IconKind,
        interactive::{Interactive, RolePalette, StateRoles, mount_dynamic_icon},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{components::InteractiveVariant, handle::ThemeHandle, role::Role},
};

/// 轨道圆角（旧 `rounded_sm`）。
const TRACK_RADIUS: f32 = 4.0;
/// 轨道内边距（旧 `.p_1()` = 4 px）。
const TRACK_PADDING: f32 = 4.0;
/// 标签间距（旧 `.gap_1()` = 4 px）。
const TAB_GAP: f32 = 4.0;

/// 指示条动画模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndicatorAnimation {
    /// 选中标签的背景色过渡（旧默认）。
    #[default]
    Fade,
    /// 一条滑动的选中指示条。
    Slide,
}

/// 一个标签。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabItem {
    /// 显示文本（同时是可访问名）。
    pub label: SmolStr,
}

impl TabItem {
    /// 文本标签。
    #[must_use]
    pub fn new(label: impl Into<SmolStr>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

/// 标签栏参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabBarOptions {
    /// 尺寸档（高度 / 内边距 / 字号）。
    pub size: UnifiedSize,
    /// 指示条动画。
    pub indicator: IndicatorAnimation,
    /// 是否显示关闭按钮（标签数 > 1 时才画）。
    pub closable: bool,
}

impl Default for TabBarOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            indicator: IndicatorAnimation::Fade,
            closable: false,
        }
    }
}

/// 标签栏（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct TabBar {
    inner: Rc<TabBarInner>,
}

struct TabBarInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: TabBarOptions,
    scope: SignalScope,
    selected: Signal<usize>,
    labels: Vec<TabItem>,
    tabs: RefCell<Vec<Interactive>>,
    close_buttons: RefCell<Vec<Option<Interactive>>>,
    on_change: RefCell<Option<Rc<dyn Fn(usize, &mut App)>>>,
    on_close: RefCell<Option<Rc<dyn Fn(usize, &mut App)>>>,
    actions: TabActions,
    indicator_left: Transient<f32>,
    indicator_width: Transient<f32>,
    indicator_height: Transient<f32>,
    indicator_target: Cell<Option<(f32, f32, f32)>>,
    indicator_started: Cell<bool>,
    geometry: RefCell<Geometry>,
}

struct TabActions {
    left: Action,
    right: Action,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    track: Option<NodeId>,
    row: Option<NodeId>,
    indicator: Option<NodeId>,
    tabs: Vec<NodeId>,
}

impl TabBar {
    /// 建标签栏。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        labels: impl IntoIterator<Item = TabItem>,
        options: TabBarOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        Self {
            inner: Rc::new(TabBarInner {
                selected: app.signal_scoped(scope, 0usize),
                indicator_left: app.transient_scoped(scope, 0.0f32),
                indicator_width: app.transient_scoped(scope, 0.0f32),
                indicator_height: app.transient_scoped(scope, 0.0f32),
                indicator_target: Cell::new(None),
                indicator_started: Cell::new(false),
                actions: TabActions {
                    left: Action::Custom(format_smolstr!("{key}.left")),
                    right: Action::Custom(format_smolstr!("{key}.right")),
                },
                labels: labels.into_iter().collect(),
                tabs: RefCell::new(Vec::new()),
                close_buttons: RefCell::new(Vec::new()),
                on_change: RefCell::new(None),
                on_close: RefCell::new(None),
                geometry: RefCell::new(Geometry::default()),
                theme,
                options,
                scope,
                key,
            }),
        }
    }

    /// 选中变化回调（用户点击 / 键盘；程序化 `set_selected` 不触发）。
    #[must_use]
    pub fn on_change(self, callback: impl Fn(usize, &mut App) + 'static) -> Self {
        *self.inner.on_change.borrow_mut() = Some(Rc::new(callback));
        self
    }

    /// 关闭回调（只有 `closable` 时会被调用）。
    #[must_use]
    pub fn on_close(self, callback: impl Fn(usize, &mut App) + 'static) -> Self {
        *self.inner.on_close.borrow_mut() = Some(Rc::new(callback));
        self
    }

    /// 当前选中下标。
    #[must_use]
    pub fn selected_index(&self, app: &App) -> usize {
        app.read_with(&self.inner.selected, |selected| *selected)
    }

    /// 标签数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.labels.len()
    }

    /// 是否没有标签。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.labels.is_empty()
    }

    /// 标签节点（`build` 之后可用；测试 / 组合控件用）。
    #[must_use]
    pub fn tab_nodes(&self) -> Vec<NodeId> {
        self.inner.geometry.borrow().tabs.clone()
    }

    /// 轨道节点。
    #[must_use]
    pub fn track_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().track
    }

    /// 指示条节点（`Fade` 模式为 `None`）。
    #[must_use]
    pub fn indicator_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().indicator
    }

    /// 指示条当前矩形（相对行；`Fade` 模式为 `None`）。
    #[must_use]
    pub fn indicator_rect(&self, app: &App) -> Option<Rect> {
        let geometry = self.inner.geometry.borrow();
        let window = geometry.window?;
        let indicator = geometry.indicator?;
        app.window(window)?
            .tree()
            .get(indicator)
            .ok()?
            .layout_override()
            .map(|layout| layout.rect)
    }

    /// 程序化选中（不触发 `on_change`；指示条动画由 `sync` 推进）。
    pub fn set_selected(&self, app: &mut App, index: usize) {
        let index = index.min(self.len().saturating_sub(1));
        app.write(&self.inner.selected, index);
    }

    /// 建节点：轨道 + 行 + 指示条 + 标签（含关闭按钮）。
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode` / 图标资源错误。
    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        if app.window(window).is_none() {
            return Err(EngineError::UnknownWindow { window });
        }
        self.inner.geometry.borrow_mut().window = Some(window);
        let inner = &self.inner;
        let count = inner.labels.len();
        let track_theme = inner.theme.clone();
        let track = NodeBuilder::boxed()
            .id(format_smolstr!("{}:track", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .width_full()
                    .padding(TRACK_PADDING)
                    .gap(TAB_GAP)
                    .build(),
            )
            .paint(PaintBuilder::new().rounded(TRACK_RADIUS).build())
            .bind_style(move |cx| StylePatch {
                background: Some(track_theme.role(cx, Role::SurfaceSunken)),
                ..StylePatch::default()
            })
            .bind_a11y(move |_cx| A11yPatch {
                size_of_set: Some(count),
                ..A11yPatch::default()
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::List),
                ..A11yOverride::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
        let row = NodeBuilder::boxed()
            .id(format_smolstr!("{}:row", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .relative()
                    .width_full()
                    .gap(TAB_GAP)
                    .build(),
            )
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                track,
            )?;
        let indicator = if inner.options.indicator == IndicatorAnimation::Slide {
            let theme = inner.theme.clone();
            Some(
                NodeBuilder::boxed()
                    .id(format_smolstr!("{}:indicator", inner.key))
                    .layout_override(Rect::default())
                    .paint(PaintBuilder::new().rounded(TRACK_RADIUS).build())
                    .bind_style(move |cx| StylePatch {
                        background: Some(theme.role(cx, Role::SurfaceOverlay)),
                        ..StylePatch::default()
                    })
                    .build(
                        app.window_mut(window)
                            .ok_or(EngineError::UnknownWindow { window })?
                            .tree_mut(),
                        row,
                    )?,
            )
        } else {
            None
        };
        let closable = inner.options.closable && count > 1;
        let mut tabs = Vec::with_capacity(count);
        let mut close_buttons = Vec::with_capacity(count);
        let mut tab_nodes = Vec::with_capacity(count);
        for (index, item) in inner.labels.iter().enumerate() {
            let close = if closable {
                let close_key = format_smolstr!("{}:close:{index}", inner.key);
                let inner_close = inner.clone();
                Some(
                    Interactive::new(app, inner.theme.clone(), close_key)
                        .variant(InteractiveVariant::Ghost)
                        .with_unified_size(UnifiedSize::Small)
                        .a11y_role(A11yRole::Button)
                        .label(format_smolstr!("Close {}", item.label))
                        .tab_stop(false)
                        .on_click(move |ctx| {
                            if let Some(callback) = inner_close.on_close.borrow().as_ref() {
                                callback(index, ctx.app_mut());
                            }
                        }),
                )
            } else {
                None
            };
            let tab_key = format_smolstr!("{}:tab:{index}", inner.key);
            let inner_select = inner.clone();
            let tab = Interactive::new(app, inner.theme.clone(), tab_key)
                .chrome(true)
                .with_unified_size(inner.options.size)
                .a11y_role(A11yRole::ListBoxOption)
                .a11y_set(index, count)
                .label(item.label.clone())
                .selected(index == 0)
                .role_palette(tab_palette())
                .on_click(move |ctx| {
                    select(&inner_select, ctx.app_mut(), index, true);
                });
            let label = item.label.clone();
            let close_for_content = close.clone();
            let close_watch = close.clone();
            let tab_for_label = tab.clone();
            let label_bind = tab.clone();
            let size = inner.options.size;
            let tab_node = tab.build(app, window, row, move |app, window, parent| {
                let text = NodeBuilder::text(label, TextStyle::new(size.text_size()))
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(label_bind.foreground_color(cx)),
                        ..StylePatch::default()
                    })
                    .build(
                        app.window_mut(window)
                            .ok_or(EngineError::UnknownWindow { window })?
                            .tree_mut(),
                        parent,
                    )?;
                tab_for_label.watch(text);
                if let Some(close) = close_for_content {
                    let close_icon = close.clone();
                    let close_bind = close.clone();
                    close.build(app, window, parent, move |app, window, parent| {
                        let icon = mount_dynamic_icon(
                            app,
                            window,
                            parent,
                            IconKind::X,
                            UnifiedSize::Small.icon_size(),
                            move |cx| StylePatch {
                                foreground: Some(close_bind.foreground_color(cx)),
                                ..StylePatch::default()
                            },
                        )?;
                        close_icon.watch(icon);
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            tabs.push(tab);
            close_buttons.push(close_watch);
            tab_nodes.push(tab_node);
        }
        *inner.tabs.borrow_mut() = tabs;
        *inner.close_buttons.borrow_mut() = close_buttons;
        {
            let mut geometry = inner.geometry.borrow_mut();
            geometry.track = Some(track);
            geometry.row = Some(row);
            geometry.indicator = indicator;
            geometry.tabs = tab_nodes;
        }
        // 标签文字色走交互前景（每个标签自己 bind）；这里给标签内容补一个可复用的主题色。
        if inner.options.indicator == IndicatorAnimation::Slide {
            self.sync_indicator_direct(app)?;
        }
        Ok(track)
    }

    /// 接引擎：注册方向键 scoped 绑定（必须在 `build` 之后）。
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode`。
    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let Some(row) = self.inner.geometry.borrow().row else {
            return Ok(());
        };
        for (code, action, step) in [
            (KeyCode::ArrowLeft, self.inner.actions.left.clone(), -1isize),
            (
                KeyCode::ArrowRight,
                self.inner.actions.right.clone(),
                1isize,
            ),
        ] {
            app.bind_key_scoped(
                window,
                row,
                KeyBinding {
                    key: code,
                    modifiers: Modifiers::empty(),
                    action: action.clone(),
                },
            )?;
            let bar = self.clone();
            app.on_action(action, move |ctx| {
                bar.move_selection(ctx, step);
            });
        }
        Ok(())
    }

    /// 一步完成 `build` + `attach`。
    ///
    /// # Errors
    /// 同 [`TabBar::build`] / [`TabBar::attach`]。
    pub fn mount(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let node = self.build(app, window, parent)?;
        self.attach(app, window)?;
        Ok(node)
    }

    /// 帧同步：标签状态、选中态与指示条动画。
    ///
    /// # Errors
    /// 同 [`Interactive::sync`] / `StaleNode`。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let selected = self.selected_index(app);
        let mut changed = false;
        for (index, tab) in self.inner.tabs.borrow().iter().enumerate() {
            changed |= tab.sync(app)?;
            changed |= tab.set_selected(app, index == selected);
        }
        for close in self.inner.close_buttons.borrow().iter().flatten() {
            changed |= close.sync(app)?;
        }
        if self.inner.options.indicator == IndicatorAnimation::Slide {
            changed |= self.sync_indicator(app)?;
        }
        Ok(changed)
    }

    /// 转发按下 / 抬起事件（每个标签与关闭按钮）。
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let mut changed = false;
        for tab in self.inner.tabs.borrow().iter() {
            changed |= tab.dispatch(app, event);
        }
        for close in self.inner.close_buttons.borrow().iter().flatten() {
            changed |= close.dispatch(app, event);
        }
        changed
    }

    /// 释放组件（方向键绑定 / Action handler / 各 Interactive 的信号作用域）。
    ///
    /// 返回释放的信号槽位数；重复调用返回 0（幂等）。
    pub fn release(&self, app: &mut App) -> usize {
        if let (Some(window), Some(row)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().row,
        ) {
            app.unbind_scope_keys(window, row);
        }
        app.off_action(&self.inner.actions.left);
        app.off_action(&self.inner.actions.right);
        let mut released = app.release_scope(self.inner.scope);
        for tab in self.inner.tabs.borrow().iter() {
            released += tab.release(app);
        }
        for close in self.inner.close_buttons.borrow().iter().flatten() {
            released += close.release(app);
        }
        self.inner.tabs.borrow_mut().clear();
        self.inner.close_buttons.borrow_mut().clear();
        released
    }

    /// 键盘移动选中（`step` = ±1；焦点跟到新标签）。
    fn move_selection(&self, ctx: &mut ActionCtx<'_>, step: isize) {
        let count = self.len();
        if count == 0 {
            return;
        }
        let current = self.selected_index(ctx.app_mut());
        let next = (current as isize + step).clamp(0, count as isize - 1) as usize;
        if next == current {
            return;
        }
        select(&self.inner, ctx.app_mut(), next, true);
        let window = ctx.window();
        if let Some(node) = self.inner.geometry.borrow().tabs.get(next).copied() {
            ctx.app_mut().set_focus(window, Some(node));
        }
    }

    /// 首次进入 `Slide`：直接把指示条放到目标位置（不播放入场动画）。
    fn sync_indicator_direct(&self, app: &mut App) -> Result<(), EngineError> {
        let Some((left, width, height)) = self.indicator_target(app) else {
            return Ok(());
        };
        app.write_transient(&self.inner.indicator_left, left);
        app.write_transient(&self.inner.indicator_width, width);
        app.write_transient(&self.inner.indicator_height, height);
        self.inner.indicator_target.set(Some((left, width, height)));
        self.inner.indicator_started.set(true);
        self.write_indicator_override(app)?;
        Ok(())
    }

    /// 每帧：目标几何变化时启动动画，然后按当前瞬态值写 `LayoutOverride`。
    fn sync_indicator(&self, app: &mut App) -> Result<bool, EngineError> {
        let Some(target) = self.indicator_target(app) else {
            return Ok(false);
        };
        let mut changed = false;
        if self.inner.indicator_target.get() != Some(target) {
            self.inner.indicator_target.set(Some(target));
            let (left, width, height) = target;
            let nodes: Vec<NodeId> = self.inner.geometry.borrow().indicator.into_iter().collect();
            let spec = AnimationSpec {
                duration: self.inner.theme.tokens().animation_tick,
                easing: Easing::EaseOutCubic,
            };
            if self.inner.indicator_started.replace(true) {
                let Some(window) = self.inner.geometry.borrow().window else {
                    return Ok(changed);
                };
                for value in [
                    AnimatedValue::Scalar {
                        target: self.inner.indicator_left,
                        to: left,
                    },
                    AnimatedValue::Scalar {
                        target: self.inner.indicator_width,
                        to: width,
                    },
                    AnimatedValue::Scalar {
                        target: self.inner.indicator_height,
                        to: height,
                    },
                ] {
                    app.animate(window, value, &nodes, spec)?;
                }
            } else {
                app.write_transient(&self.inner.indicator_left, left);
                app.write_transient(&self.inner.indicator_width, width);
                app.write_transient(&self.inner.indicator_height, height);
            }
            changed = true;
        }
        changed |= self.write_indicator_override(app)?;
        Ok(changed)
    }

    /// 把当前瞬态值写进指示条的 `LayoutOverride`（没变化则不写）。
    fn write_indicator_override(&self, app: &mut App) -> Result<bool, EngineError> {
        let (Some(window), Some(indicator)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().indicator,
        ) else {
            return Ok(false);
        };
        let rect = Rect::new(
            *app.read_transient(&self.inner.indicator_left),
            0.0,
            *app.read_transient(&self.inner.indicator_width),
            *app.read_transient(&self.inner.indicator_height),
        );
        let current = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree()
            .get(indicator)
            .ok()
            .and_then(|target| target.layout_override())
            .map(|layout| layout.rect);
        if current == Some(rect) {
            return Ok(false);
        }
        app.window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut()
            .set_layout_override(indicator, Some(LayoutOverride::new(rect)))?;
        Ok(true)
    }

    /// 选中标签相对行节点的矩形（`None` = 还没布局）。
    fn indicator_target(&self, app: &App) -> Option<(f32, f32, f32)> {
        let geometry = self.inner.geometry.borrow();
        let row = geometry.row?;
        let window = geometry.window?;
        let selected = self.selected_index(app);
        let tab = *geometry.tabs.get(selected)?;
        let tree = app.window(window)?.tree();
        let row_bounds = tree.get(row).ok()?.layout().bounds;
        let tab_bounds = tree.get(tab).ok()?.layout().bounds;
        Some((
            tab_bounds.origin.x - row_bounds.origin.x,
            tab_bounds.size.width,
            tab_bounds.size.height,
        ))
    }

    /// 标签栏 key（调试 id / 动作前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }
}

/// 选中一个标签（写信号 + 可选回调）。
fn select(inner: &Rc<TabBarInner>, app: &mut App, index: usize, notify: bool) {
    let count = inner.labels.len();
    if count == 0 {
        return;
    }
    let index = index.min(count - 1);
    if !app.write(&inner.selected, index) {
        return;
    }
    if notify && let Some(callback) = inner.on_change.borrow().as_ref() {
        callback(index, app);
    }
}

/// 标签的四状态角色配方（旧 tab_bar 的选中 / 悬停画法）。
fn tab_palette() -> RolePalette {
    RolePalette {
        normal: StateRoles {
            background: None,
            foreground: Role::FgMuted,
            border: None,
        },
        hover: StateRoles {
            background: Some(Role::VeilHover),
            foreground: Role::FgMuted,
            border: None,
        },
        active: StateRoles {
            background: Some(Role::SurfaceOverlay),
            foreground: Role::FgDefault,
            border: None,
        },
        disabled: StateRoles {
            background: None,
            foreground: Role::FgDisabled,
            border: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use ui_core::{
        a11y::A11yRole,
        event::{InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect},
        node::id::NodeId,
        paint::color::{Color, ColorResolver},
        test_support::app::TestApp,
    };

    use super::{IndicatorAnimation, TabBar, TabBarOptions, TabItem};
    use crate::{
        kit::size::UnifiedSize,
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

    struct Harness {
        app: TestApp,
        bar: TabBar,
        changed: Rc<Cell<Option<usize>>>,
        closed: Rc<Cell<Option<usize>>>,
    }

    fn harness(indicator: IndicatorAnimation, closable: bool, count: usize) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let changed = Rc::new(Cell::new(None));
        let closed = Rc::new(Cell::new(None));
        let change_sink = changed.clone();
        let close_sink = closed.clone();
        let labels = (0..count).map(|index| TabItem::new(format!("tab{index}")));
        let bar = TabBar::new(
            app.app_mut(),
            theme,
            "tabs",
            labels,
            TabBarOptions {
                size: UnifiedSize::Medium,
                indicator,
                closable,
            },
        )
        .on_change(move |index, _| change_sink.set(Some(index)))
        .on_close(move |index, _| close_sink.set(Some(index)));
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        bar.mount(app.app_mut(), window, root).expect("mount");
        let mut harness = Harness {
            app,
            bar,
            changed,
            closed,
        };
        settle(&mut harness);
        harness
    }

    fn settle(harness: &mut Harness) {
        for _ in 0..5 {
            harness.app.advance(Duration::from_millis(60));
            let _ = harness.app.drain().expect("frame");
            let _ = harness.bar.sync(harness.app.app_mut());
        }
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.bar.sync(harness.app.app_mut());
        let _ = harness.app.drain().expect("frame");
    }

    fn click(app: &mut TestApp, node: NodeId) {
        let bounds = app.probe().bounds(node).expect("bounds");
        let point = Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    fn tab_patch(harness: &Harness, index: usize) -> ui_core::paint::style::StylePatch {
        let window = harness.app.window_id();
        let node = harness.bar.tab_nodes()[index];
        harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("tab")
            .style_patch()
            .clone()
    }

    fn resolved(color: Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    /// 默认选中第一个；点击第二个标签选中它并回调。
    #[test]
    fn clicking_a_tab_selects_it_and_reports_the_index() {
        let mut harness = harness(IndicatorAnimation::Fade, false, 3);
        assert_eq!(harness.bar.selected_index(harness.app.app()), 0);
        let node = harness.bar.tab_nodes()[1];
        click(&mut harness.app, node);
        let _ = harness.bar.sync(harness.app.app_mut());
        assert_eq!(harness.bar.selected_index(harness.app.app()), 1);
        assert_eq!(harness.changed.get(), Some(1));
    }

    /// 方向键（E8 scoped）：焦点在标签栏子树内时 Right/Left 移动选中并跟焦点。
    #[test]
    fn arrow_keys_move_the_selection_within_the_scoped_tab_bar() {
        let mut harness = harness(IndicatorAnimation::Fade, false, 3);
        let window = harness.app.window_id();
        let first = harness.bar.tab_nodes()[0];
        assert!(harness.app.app_mut().set_focus(window, Some(first)));
        let _ = harness.app.inject(InputEvent::key_down(
            KeyCode::ArrowRight,
            Modifiers::empty(),
        ));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::ArrowRight, Modifiers::empty()));
        assert_eq!(harness.bar.selected_index(harness.app.app()), 1);
        assert_eq!(
            harness.app.app().focused(window),
            Some(harness.bar.tab_nodes()[1]),
            "焦点跟到新标签"
        );
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::ArrowLeft, Modifiers::empty()));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::ArrowLeft, Modifiers::empty()));
        assert_eq!(harness.bar.selected_index(harness.app.app()), 0);
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::ArrowLeft, Modifiers::empty()));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::ArrowLeft, Modifiers::empty()));
        assert_eq!(
            harness.bar.selected_index(harness.app.app()),
            0,
            "左边界不环绕"
        );
    }

    /// Slide：指示条最初覆盖第一个标签，选中最后一个后移动到它上面（变异：不写 LayoutOverride → 红）。
    #[test]
    fn the_slide_indicator_moves_to_the_selected_tab() {
        let mut harness = harness(IndicatorAnimation::Slide, false, 3);
        let row = harness
            .bar
            .indicator_rect(harness.app.app())
            .expect("indicator");
        let first = harness.bar.tab_nodes()[0];
        let row_node = harness
            .app
            .app()
            .window(harness.app.window_id())
            .expect("window")
            .tree()
            .get(first)
            .expect("tab")
            .parent()
            .expect("row");
        let row_bounds = harness.app.probe().bounds(row_node).expect("row");
        let first_bounds = harness.app.probe().bounds(first).expect("tab");
        let expected_first = Rect::new(
            first_bounds.origin.x - row_bounds.origin.x,
            0.0,
            first_bounds.size.width,
            first_bounds.size.height,
        );
        assert!(
            (row.origin.x - expected_first.origin.x).abs() < 0.5
                && (row.size.width - expected_first.size.width).abs() < 0.5,
            "初始指示条应覆盖第一个标签（实际 {row:?} / 期望 {expected_first:?}）"
        );

        harness.bar.set_selected(harness.app.app_mut(), 2);
        settle(&mut harness);
        let rect = harness
            .bar
            .indicator_rect(harness.app.app())
            .expect("indicator");
        let last = harness.bar.tab_nodes()[2];
        let last_bounds = harness.app.probe().bounds(last).expect("tab");
        let expected_last = Rect::new(
            last_bounds.origin.x - row_bounds.origin.x,
            0.0,
            last_bounds.size.width,
            last_bounds.size.height,
        );
        assert!(
            (rect.origin.x - expected_last.origin.x).abs() < 0.5,
            "指示条必须移动到最后一个标签（实际 {} / 期望 {}）",
            rect.origin.x,
            expected_last.origin.x
        );
        assert!(
            (rect.size.width - expected_last.size.width).abs() < 0.5,
            "指示条宽度跟随标签"
        );
    }

    /// Fade：没有指示条节点；选中标签的背景是 `surface.overlay`。
    #[test]
    fn fade_mode_paints_the_selected_tab_with_the_overlay_role() {
        let mut harness = harness(IndicatorAnimation::Fade, false, 3);
        assert!(harness.bar.indicator_node().is_none());
        let theme = Theme::of(Appearance::Light);
        assert_eq!(
            tab_patch(&harness, 0).background.map(resolved),
            Some(resolved(theme.role(Role::SurfaceOverlay))),
            "选中标签的背景"
        );
        let _ = harness.bar.sync(harness.app.app_mut());
        let node = harness.bar.tab_nodes()[2];
        click(&mut harness.app, node);
        settle(&mut harness);
        assert_eq!(
            tab_patch(&harness, 2).background.map(resolved),
            Some(resolved(theme.role(Role::SurfaceOverlay)))
        );
        assert_eq!(
            tab_patch(&harness, 0).background.map(resolved),
            Some(resolved(Color::TRANSPARENT)),
            "未选中标签透明"
        );
    }

    /// a11y：轨道是 List（集合大小），标签是 ListBoxOption（位置 / 选中）。
    #[test]
    fn the_tab_bar_declares_list_and_list_item_semantics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let bar = TabBar::new(
            app.app_mut(),
            theme,
            "a11y-tabs",
            [
                TabItem::new("one"),
                TabItem::new("two"),
                TabItem::new("three"),
            ],
            TabBarOptions {
                size: UnifiedSize::Medium,
                indicator: IndicatorAnimation::Fade,
                closable: false,
            },
        );
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        bar.mount(app.app_mut(), window, root).expect("mount");
        let _ = app.drain().expect("a11y frame");
        let snapshot = app.a11y().expect("a11y");
        let track = snapshot
            .node_for(bar.track_node().expect("track"))
            .expect("track a11y");
        assert_eq!(track.role, A11yRole::List);
        assert_eq!(track.size_of_set, Some(3));
        let tabs = bar.tab_nodes();
        let second = snapshot.node_for(tabs[1]).expect("tab a11y");
        assert_eq!(second.role, A11yRole::ListBoxOption);
        assert_eq!(second.pos_in_set, Some(1));
        assert_eq!(second.size_of_set, Some(3));
        assert_eq!(second.selected, Some(false));
        assert_eq!(second.label_text(), Some("two"));
        let first = snapshot.node_for(tabs[0]).expect("tab a11y");
        assert_eq!(first.selected, Some(true));
    }

    /// 关闭按钮：点 X 只调 `on_close`，不改变选中；只剩一个标签时不画 X。
    #[test]
    fn the_close_button_reports_the_index_without_selecting() {
        let mut multi = harness(IndicatorAnimation::Fade, true, 3);
        let window = multi.app.window_id();
        let tab = multi.bar.tab_nodes()[1];
        let close = multi
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(tab)
            .expect("tab")
            .children()
            .last()
            .copied()
            .expect("close button");
        click(&mut multi.app, close);
        assert_eq!(multi.closed.get(), Some(1));
        assert_eq!(multi.bar.selected_index(multi.app.app()), 0);

        let single = harness(IndicatorAnimation::Fade, true, 1);
        let window = single.app.window_id();
        let children = single
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(single.bar.tab_nodes()[0])
            .expect("tab")
            .children()
            .len();
        assert_eq!(children, 1, "只剩一个标签时只画文字");
    }

    /// I3：动画结束后的稳态不出帧。
    #[test]
    fn a_settled_tab_bar_produces_no_further_frames() {
        let mut harness = harness(IndicatorAnimation::Slide, false, 3);
        harness.bar.set_selected(harness.app.app_mut(), 2);
        settle(&mut harness);
        assert!(
            !harness.bar.sync(harness.app.app_mut()).expect("sync"),
            "稳态 sync 返回 false"
        );
        let record = harness.app.drain().expect("idle").clone();
        assert!(!record.rendered, "稳态不得出帧（I3）");
    }
}
