//! 自绘滚动条（P2-2）。
//!
//! 引擎没有滚动条控件（评估 G5），按 A2/D-09 的结论由 `base_ui` 自绘：轨道 + 滑块两个普通节点，
//! 几何由 [`ScrollState`](ui_core::interaction::ScrollState) 的 `offset / content / viewport`
//! 现算，滑块位置写 `LayoutOverride`（相对轨道的 border box），颜色 / 淡出走 `bind_style` +
//! `Transient`。
//!
//! # 交互
//!
//! - **滑块拖拽**：`on_drag` + `capture_pointer`；位移按 `可拖动长度 / 可滚动范围` 比例换算成
//!   偏移，走唯一写点 `App::scroll_to`（引擎负责 clamp / damage / 取消滚轮动画）。
//! - **轨道点击**：按点击位置把滑块中心移过去（与 Qt `QScrollBar` 的页面跳转同语义）。
//! - **悬停 / 淡出**：`ScrollbarMode::Hover` 时滑块 `Transient<f32>` 不透明度在悬停 / 拖拽时
//!   动画到 1、离开后回到 0（`App::animate` + `AnimationSpec::short()`）；`Always` 恒为 1。
//!   悬停态由 [`ScrollBar::sync`] 读 `App::hovered` 镜像到 `Signal<bool>`（引擎的 hover 不进
//!   `ReadCtx`，与本批列表的悬停镜像同一机制）。动画只在**目标变化**时启动：引擎的
//!   `animate` 每次都用当前值作新起点并重置时长，逐帧重启会把 120 ms 拉长到 0.4–0.5 s。
//!
//! # 模式
//!
//! [`ScrollbarMode::Always`] 恒显示、[`ScrollbarMode::Hover`] 悬停 / 拖拽时淡入。旧
//! 旧组件库的第三种模式 `Scrolling`（滚动时显示、空闲后淡出）本批**未实现**：它需要
//! 时钟 / 超时状态与帧源接入（滚动停止后还要有一次帧来启动淡出），属后续批次；登记为范围
//! 缺口（P3-01），旧表 / 菜单 / 侧栏在 widget 迁移前依赖默认模式的调用点需要显式选择
//! `Always` 或 `Hover`。
//!
//! # 生命周期
//!
//! 组件的信号与瞬态值归属**自有** `SignalScope`（[`ScrollBar::release`] 释放）。动态创建 /
//! 销毁的滚动条在卸载节点后调用一次；引用淡出瞬态值的动画槽由引擎在释放时同批取消。
//!
//! # 轴
//!
//! 引擎滚动容器是**单轴**的（`ScrollAxis`），滚动条因此也按轴各建一个：纵向列表 / 表格用
//! [`ScrollbarAxis::Vertical`]，横向图表 / 表头用 [`ScrollbarAxis::Horizontal`]。
//!
//! # 帧同步
//!
//! [`ScrollBar::sync`] 是帧驱动入口（组合根在 `on_frame` 钩子里调用；测试直接调）。滚动条几何
//! 与淡出**只在变化时**写：偏移不变时 `LayoutOverride` 等值 no-op、`Transient` 已到目标时不再
//! `animate`（I3：静止无帧）。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::{Action, ActionSource},
    interaction::{ActionCtx, AnimatedValue, AnimationSpec, DragPhase, ScrollHandle},
    layout::LayoutOverride,
    math::{Point, Rect, Size},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
        tree::NodeTree,
    },
    paint::style::StylePatch,
    window::WindowId,
};

use crate::{
    style_builder::{PaintBuilder, StyleBuilder},
    theme::handle::ThemeHandle,
};

/// 滚动条轴（与引擎滚动容器的单轴契约一致）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarAxis {
    /// 纵向（右侧竖条）。
    Vertical,
    /// 横向（底部横条）。
    Horizontal,
}

impl ScrollbarAxis {
    fn length(self, size: Size) -> f32 {
        match self {
            Self::Vertical => size.height,
            Self::Horizontal => size.width,
        }
    }

    fn component(self, point: Point) -> f32 {
        match self {
            Self::Vertical => point.y,
            Self::Horizontal => point.x,
        }
    }

    fn point(self, offset: f32) -> Point {
        match self {
            Self::Vertical => Point::new(0.0, offset),
            Self::Horizontal => Point::new(offset, 0.0),
        }
    }
}

/// 显示模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarMode {
    /// 恒显示（列表 / 表格默认）。
    Always,
    /// 指针在滚动条上或拖动时才显示（淡入淡出）。
    Hover,
    // 旧组件库的 `Scrolling`（滚动时显示、空闲淡出）未实现：见模块文档〈模式〉。
}

/// 滚动条参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarOptions {
    /// 滑块厚度（逻辑像素；旧 `scroll_bar.rs` 的 `width(4.)`）。
    pub thickness: f32,
    /// 滑块圆角。
    pub corner_radius: f32,
    /// 滑块最小长度（内容极长时不至于点不到）。
    pub min_thumb: f32,
    /// 显示模式。
    pub mode: ScrollbarMode,
}

impl Default for ScrollbarOptions {
    fn default() -> Self {
        Self {
            thickness: 4.0,
            corner_radius: 1.0,
            min_thumb: 20.0,
            mode: ScrollbarMode::Always,
        }
    }
}

impl ScrollbarOptions {
    /// 悬停淡出模式。
    #[must_use]
    pub fn hover(mut self) -> Self {
        self.mode = ScrollbarMode::Hover;
        self
    }

    /// 滑块厚度。
    #[must_use]
    pub fn thickness(mut self, thickness: f32) -> Self {
        self.thickness = thickness.max(1.0);
        self
    }
}

/// 拖拽开始时的快照（瞬态值：不失效、只给拖拽换算用）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DragStart {
    start_offset: f32,
    max: f32,
    usable: f32,
}

struct ScrollBarInner {
    key: SmolStr,
    theme: ThemeHandle,
    axis: ScrollbarAxis,
    options: ScrollbarOptions,
    /// 组件自有信号作用域（动态创建 / 销毁的滚动条释放它回收槽位）。
    scope: SignalScope,
    hovered: Signal<bool>,
    scrollable: Signal<bool>,
    fade: Transient<f32>,
    /// 淡出动画**当前目标**：只在目标变化时启动动画。
    ///
    /// 旧实现在动画在飞时（`current != target`）每帧重新 `animate`，而引擎每次重启都会用
    /// **当前值**作新起点、重置时钟 —— 120 ms 的动画被拉长到 0.4–0.5 s。这里记住目标，
    /// 稳态（目标没变）不再启动。
    fade_target: Cell<Option<f32>>,
    drag: Transient<Option<DragStart>>,
    geometry: RefCell<Geometry>,
    track_action: Action,
    thumb_action: Action,
    drag_action: Action,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    track: Option<NodeId>,
    thumb: Option<NodeId>,
    handle: Option<ScrollHandle>,
}

/// 自绘滚动条句柄（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct ScrollBar {
    inner: Rc<ScrollBarInner>,
}

impl ScrollBar {
    /// 建滚动条状态。`build` 建节点、`attach` 接滚动容器。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        axis: ScrollbarAxis,
        options: ScrollbarOptions,
    ) -> Self {
        let key = key.into();
        let initial_fade = if options.mode == ScrollbarMode::Always {
            1.0
        } else {
            0.0
        };
        let scope = app.signal_scope();
        Self {
            inner: Rc::new(ScrollBarInner {
                scope,
                hovered: app.signal_scoped(scope, false),
                scrollable: app.signal_scoped(scope, false),
                fade: app.transient_scoped(scope, initial_fade),
                fade_target: Cell::new(None),
                drag: app.transient_scoped(scope, None),
                geometry: RefCell::new(Geometry::default()),
                track_action: Action::Custom(format_smolstr!("{key}.track")),
                thumb_action: Action::Custom(format_smolstr!("{key}.thumb")),
                drag_action: Action::Custom(format_smolstr!("{key}.drag")),
                theme,
                axis,
                options,
                key,
            }),
        }
    }

    /// 只建节点：轨道（绝对定位）+ 滑块（`LayoutOverride`）。
    ///
    /// # Errors
    /// `StaleNode`（`parent` 陈旧）。
    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let inner = &self.inner;
        let thickness = inner.options.thickness;
        let mut track_style = StyleBuilder::new().absolute();
        track_style = match inner.axis {
            ScrollbarAxis::Vertical => track_style.top(0.0).right(0.0).bottom(0.0).width(thickness),
            ScrollbarAxis::Horizontal => track_style
                .left(0.0)
                .right(0.0)
                .bottom(0.0)
                .height(thickness),
        };
        let track = NodeBuilder::boxed()
            .id(format_smolstr!("{}:track", inner.key))
            .style(track_style.build())
            .on_click(inner.track_action.clone())
            .build(tree, parent)?;
        let initial = match inner.axis {
            ScrollbarAxis::Vertical => Rect::new(0.0, 0.0, thickness, inner.options.min_thumb),
            ScrollbarAxis::Horizontal => Rect::new(0.0, 0.0, inner.options.min_thumb, thickness),
        };
        let theme = inner.theme.clone();
        let hovered = inner.hovered;
        let scrollable = inner.scrollable;
        let fade = inner.fade;
        let thumb = NodeBuilder::boxed()
            .id(format_smolstr!("{}:thumb", inner.key))
            .layout_override(initial)
            .paint(
                PaintBuilder::new()
                    .rounded(inner.options.corner_radius)
                    .build(),
            )
            .on_click(inner.thumb_action.clone())
            .on_drag(inner.drag_action.clone())
            .bind_style(move |cx| {
                let active = *cx.read(&hovered);
                let visible = *cx.read(&scrollable);
                let opacity = *cx.read_transient(&fade);
                let color = {
                    let components = theme.components(cx);
                    if active {
                        components.scrollbar.active
                    } else {
                        components.scrollbar.normal
                    }
                };
                StylePatch {
                    background: Some(color),
                    opacity: Some(opacity),
                    visible: Some(visible),
                    ..StylePatch::default()
                }
            })
            .build(tree, track)?;
        let mut geometry = inner.geometry.borrow_mut();
        geometry.track = Some(track);
        geometry.thumb = Some(thumb);
        Ok(track)
    }

    /// 接滚动容器：取 `ScrollHandle` 并注册交互。
    pub fn attach(&self, app: &mut App, window: WindowId, scroll_node: NodeId) {
        let handle = app.scroll_handle(window, scroll_node);
        {
            let mut geometry = self.inner.geometry.borrow_mut();
            geometry.window = Some(window);
            geometry.handle = handle;
        }
        let track = self.clone();
        app.on_action(self.inner.track_action.clone(), move |ctx| {
            track.on_track_click(ctx);
        });
        app.on_action(self.inner.thumb_action.clone(), move |_ctx| {
            // 滑块上的点击/拖拽已经消费了事件；保留一个空 handler 是为了让命中**停在这里**
            // （动作沿命中节点 → 祖先冒泡取第一条），否则同一次点击还会触发轨道的页面跳转。
        });
        let drag = self.clone();
        app.on_action(self.inner.drag_action.clone(), move |ctx| {
            drag.on_thumb_drag(ctx);
        });
    }

    /// 释放本组件的信号作用域与引擎侧注册（动态创建 / 销毁的滚动条调用；先卸载节点再释放）。
    ///
    /// 清理面：轨道 / 滑块 / 拖拽三个 Action handler（[`App::off_action`]）+ 本组件的信号作用域。
    /// 只释放信号不摘 handler 会让 `App` 永久持有 `Rc<ScrollBar>` 闭包（P2-2 复核 R2.2 的泄漏）。
    /// 返回被释放的槽位数（悬停 / 可滚动 / 淡出不透明度 / 拖拽快照）；引用淡出瞬态值的动画
    /// 由引擎在释放时同批取消。重复调用返回 0（幂等）。
    pub fn release(&self, app: &mut App) -> usize {
        app.off_action(&self.inner.track_action);
        app.off_action(&self.inner.thumb_action);
        app.off_action(&self.inner.drag_action);
        app.release_scope(self.inner.scope)
    }

    /// 轨道 / 滑块节点（测试用）。
    #[must_use]
    pub fn nodes(&self) -> (Option<NodeId>, Option<NodeId>) {
        let geometry = self.inner.geometry.borrow();
        (geometry.track, geometry.thumb)
    }

    /// 滑块当前布局矩形（相对轨道；测试用）。
    #[must_use]
    pub fn thumb_rect(&self, app: &App) -> Option<Rect> {
        let geometry = self.inner.geometry.borrow();
        let (window, thumb) = (geometry.window?, geometry.thumb?);
        drop(geometry);
        app.window(window)?
            .tree()
            .get(thumb)
            .ok()?
            .layout_override()
            .map(|layout| layout.rect)
    }

    /// 滑块不透明度（瞬态值；测试用）。
    #[must_use]
    pub fn opacity(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.fade)
    }

    /// 不透明度瞬态值（组合控件 / 测试可读）。
    #[must_use]
    pub fn opacity_transient(&self) -> Transient<f32> {
        self.inner.fade
    }

    /// 滑块几何状态是否可滚动（测试用）。
    #[must_use]
    pub fn scrollable(&self, app: &App) -> bool {
        app.read_with(&self.inner.scrollable, |scrollable| *scrollable)
    }

    /// 帧同步：几何 + 悬停镜像 + 淡出（无变化 ⇒ `false`，不请求帧）。
    ///
    /// # Errors
    /// `StaleNode`（节点已卸载）/ `UnknownWindow`。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let (Some(window), Some(track), Some(thumb), Some(handle)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().track,
            self.inner.geometry.borrow().thumb,
            self.inner.geometry.borrow().handle,
        ) else {
            return Ok(false);
        };
        let Some(state) = app.scroll_state(&handle) else {
            return Ok(false);
        };
        let Some(target) = app.window(window) else {
            return Ok(false);
        };
        let mut changed = false;
        let (track_len, current) = {
            let tree = target.tree();
            let track_bounds = tree.get(track)?.layout().bounds;
            (
                self.inner.axis.length(track_bounds.size),
                tree.get(thumb).ok().and_then(|node| node.layout_override()),
            )
        };
        let viewport = self.inner.axis.length(state.viewport);
        let content = self.inner.axis.length(state.content);
        let offset = self.inner.axis.component(state.offset);
        let max = (content - viewport).max(0.0);
        let scrollable = max > 0.5;
        let rect = thumb_rect(
            self.inner.axis,
            track_len,
            self.inner.options,
            viewport,
            content,
            offset,
        );
        if current.map(|layout| layout.rect) != Some(rect) {
            let Some(target) = app.window_mut(window) else {
                return Ok(false);
            };
            target
                .tree_mut()
                .set_layout_override(thumb, Some(LayoutOverride::new(rect)))?;
            changed = true;
        }
        if app.write(&self.inner.scrollable, scrollable) {
            changed = true;
        }
        let hovered = node_in_subtree(app, window, track, app.hovered(window));
        if app.write(&self.inner.hovered, hovered) {
            changed = true;
        }
        let dragging = (*app.read_transient(&self.inner.drag)).is_some();
        let target_opacity = match self.inner.options.mode {
            ScrollbarMode::Always => f32::from(scrollable),
            ScrollbarMode::Hover => {
                if scrollable && (hovered || dragging) {
                    1.0
                } else {
                    0.0
                }
            }
        };
        let current_opacity = *app.read_transient(&self.inner.fade);
        // 只在**目标变化**时启动动画：目标不变时（动画在飞）不重启，否则引擎每次都用当前值
        // 作新起点并重置时钟，120 ms 的规格会被拖长到 0.4–0.5 s（P2-2b）。
        if self.inner.fade_target.get() != Some(target_opacity) {
            if (current_opacity - target_opacity).abs() > 1.0e-4 {
                app.animate(
                    window,
                    AnimatedValue::Scalar {
                        target: self.inner.fade,
                        to: target_opacity,
                    },
                    &[thumb],
                    AnimationSpec::short(),
                )?;
                changed = true;
            }
            self.inner.fade_target.set(Some(target_opacity));
        }
        Ok(changed)
    }

    fn on_track_click(&self, ctx: &mut ActionCtx<'_>) {
        let Some(hit) = ctx.hit() else {
            return;
        };
        let (Some(handle), Some(track)) = (
            self.inner.geometry.borrow().handle,
            self.inner.geometry.borrow().track,
        ) else {
            return;
        };
        let window = ctx.window();
        let (state, track_len) = {
            let Some(state) = ctx.app_mut().scroll_state(&handle) else {
                return;
            };
            let Some(target) = ctx.app_mut().window(window) else {
                return;
            };
            let track_len = target
                .tree()
                .get(track)
                .ok()
                .map(|node| self.inner.axis.length(node.layout().bounds.size))
                .unwrap_or(0.0);
            (state, track_len)
        };
        let viewport = self.inner.axis.length(state.viewport);
        let content = self.inner.axis.length(state.content);
        let max = (content - viewport).max(0.0);
        let thumb_len = thumb_length(track_len, viewport, content, self.inner.options);
        let click = self.inner.axis.component(hit.local_point);
        let target_offset = track_target_offset(track_len, thumb_len, max, click);
        let _ = ctx
            .app_mut()
            .scroll_to(&handle, self.inner.axis.point(target_offset));
    }

    fn on_thumb_drag(&self, ctx: &mut ActionCtx<'_>) {
        let (Some(window), Some(thumb), Some(handle)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().thumb,
            self.inner.geometry.borrow().handle,
        ) else {
            return;
        };
        let ActionSource::Drag { phase, delta } = ctx.source() else {
            return;
        };
        match phase {
            DragPhase::Start => {
                let (Some(track), Some(state)) = (
                    self.inner.geometry.borrow().track,
                    ctx.app_mut().scroll_state(&handle),
                ) else {
                    return;
                };
                let track_len = ctx
                    .app_mut()
                    .window(window)
                    .and_then(|target| target.tree().get(track).ok())
                    .map(|node| self.inner.axis.length(node.layout().bounds.size))
                    .unwrap_or(0.0);
                let viewport = self.inner.axis.length(state.viewport);
                let content = self.inner.axis.length(state.content);
                let max = (content - viewport).max(0.0);
                let thumb_len = thumb_length(track_len, viewport, content, self.inner.options);
                let usable = (track_len - thumb_len).max(0.0);
                let start = DragStart {
                    start_offset: self.inner.axis.component(state.offset),
                    max,
                    usable,
                };
                ctx.app_mut().write_transient(&self.inner.drag, Some(start));
                let _ = ctx.app_mut().capture_pointer(window, thumb);
            }
            DragPhase::Move => {
                let start = *ctx.app_mut().read_transient(&self.inner.drag);
                let Some(start) = start else {
                    return;
                };
                let moved = self.inner.axis.component(delta);
                let target = drag_offset(start.start_offset, moved, start.usable, start.max);
                let _ = ctx
                    .app_mut()
                    .scroll_to(&handle, self.inner.axis.point(target));
            }
            DragPhase::End | DragPhase::Cancel => {
                ctx.app_mut().write_transient(&self.inner.drag, None);
                let _ = ctx.app_mut().release_pointer(window);
            }
        }
    }
}

/// 滑块长度：轨道长度 × 视口 / 内容，夹在 `[min_thumb, track_len]`。
fn thumb_length(track_len: f32, viewport: f32, content: f32, options: ScrollbarOptions) -> f32 {
    if track_len <= 0.0 || content <= 0.0 {
        return 0.0;
    }
    let ratio = (viewport / content).clamp(0.0, 1.0);
    (track_len * ratio)
        .max(options.min_thumb.min(track_len))
        .min(track_len)
}

fn thumb_offset(track_len: f32, thumb_len: f32, offset: f32, max: f32) -> f32 {
    let usable = (track_len - thumb_len).max(0.0);
    if max <= 0.0 || usable <= 0.0 {
        return 0.0;
    }
    (offset.clamp(0.0, max) / max * usable).clamp(0.0, usable)
}

fn thumb_rect(
    axis: ScrollbarAxis,
    track_len: f32,
    options: ScrollbarOptions,
    viewport: f32,
    content: f32,
    offset: f32,
) -> Rect {
    let max = (content - viewport).max(0.0);
    let len = thumb_length(track_len, viewport, content, options);
    let pos = thumb_offset(track_len, len, offset, max);
    match axis {
        ScrollbarAxis::Vertical => Rect::new(0.0, pos, options.thickness, len),
        ScrollbarAxis::Horizontal => Rect::new(pos, 0.0, len, options.thickness),
    }
}

/// 轨道点击的目标偏移：把滑块中心移到点击位置。
fn track_target_offset(track_len: f32, thumb_len: f32, max: f32, click: f32) -> f32 {
    let usable = (track_len - thumb_len).max(0.0);
    if usable <= 0.0 || max <= 0.0 {
        return 0.0;
    }
    ((click - thumb_len / 2.0) / usable).clamp(0.0, 1.0) * max
}

/// 拖拽换算：起点偏移 + 位移 × 可滚动范围 / 可拖动长度。
fn drag_offset(start: f32, delta: f32, usable: f32, max: f32) -> f32 {
    if max <= 0.0 {
        return start.max(0.0);
    }
    if usable <= 0.0 {
        return start.clamp(0.0, max);
    }
    (start + delta / usable * max).clamp(0.0, max)
}

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

#[cfg(test)]
mod tests {
    use ui_core::math::Rect;

    use super::{
        ScrollBar, ScrollbarAxis, ScrollbarMode, ScrollbarOptions, drag_offset, thumb_length,
        thumb_offset, thumb_rect, track_target_offset,
    };

    fn options() -> ScrollbarOptions {
        ScrollbarOptions::default()
    }

    #[test]
    fn thumb_length_follows_the_visible_fraction_and_clamps() {
        assert_eq!(thumb_length(200.0, 100.0, 400.0, options()), 50.0);
        assert_eq!(
            thumb_length(200.0, 10.0, 10_000.0, options()),
            20.0,
            "极长内容夹到最小长度"
        );
        assert_eq!(
            thumb_length(10.0, 100.0, 100.0, options()),
            10.0,
            "内容不足一屏 ⇒ 滑块占满轨道"
        );
        assert_eq!(thumb_length(0.0, 100.0, 400.0, options()), 0.0);
    }

    #[test]
    fn thumb_offset_maps_the_scroll_range_onto_the_track() {
        assert_eq!(thumb_offset(200.0, 50.0, 0.0, 300.0), 0.0);
        assert_eq!(thumb_offset(200.0, 50.0, 150.0, 300.0), 75.0);
        assert_eq!(thumb_offset(200.0, 50.0, 300.0, 300.0), 150.0);
        assert_eq!(
            thumb_offset(200.0, 50.0, 10_000.0, 300.0),
            150.0,
            "越界 clamp"
        );
        assert_eq!(thumb_offset(200.0, 200.0, 10.0, 0.0), 0.0, "不可滚动");
    }

    #[test]
    fn thumb_rect_covers_both_axes() {
        assert_eq!(
            thumb_rect(
                ScrollbarAxis::Vertical,
                200.0,
                options(),
                100.0,
                400.0,
                200.0
            ),
            Rect::new(0.0, 100.0, 4.0, 50.0)
        );
        assert_eq!(
            thumb_rect(
                ScrollbarAxis::Horizontal,
                200.0,
                options(),
                100.0,
                400.0,
                0.0
            ),
            Rect::new(0.0, 0.0, 50.0, 4.0)
        );
    }

    #[test]
    fn track_click_centers_the_thumb() {
        // 轨道 200、滑块 50、范围 300：点击 100 处 ⇒ 滑块中心在 100 ⇒ 可拖中点 ⇒ 目标 150。
        assert_eq!(track_target_offset(200.0, 50.0, 300.0, 100.0), 150.0);
        assert_eq!(track_target_offset(200.0, 50.0, 300.0, 0.0), 0.0);
        assert_eq!(track_target_offset(200.0, 50.0, 300.0, 200.0), 300.0);
        assert_eq!(track_target_offset(200.0, 200.0, 300.0, 100.0), 0.0);
    }

    #[test]
    fn drag_moves_by_the_track_to_content_ratio() {
        // 可拖动 150、范围 300：拖 75 ⇒ 偏移 150。
        assert_eq!(drag_offset(0.0, 75.0, 150.0, 300.0), 150.0);
        assert_eq!(drag_offset(100.0, -150.0, 150.0, 300.0), 0.0);
        assert_eq!(drag_offset(100.0, 10_000.0, 150.0, 300.0), 300.0);
        assert_eq!(drag_offset(42.0, 5.0, 0.0, 0.0), 42.0, "不可拖动时保持");
    }

    #[test]
    fn sync_without_attach_is_a_no_op() {
        let mut app = ui_core::test_support::app::TestApp::new();
        let theme = crate::theme::handle::ThemeHandle::new(
            app.app_mut(),
            crate::theme::appearance::Appearance::Light,
        );
        let bar = ScrollBar::new(
            app.app_mut(),
            theme,
            "scrollbar",
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        assert!(!bar.sync(app.app_mut()).expect("sync"));
        assert_eq!(bar.nodes(), (None, None));
        let _ = app.drain().expect("frame");
    }

    #[test]
    fn opacity_starts_at_one_for_always_and_zero_for_hover() {
        let mut app = ui_core::test_support::app::TestApp::new();
        let theme = crate::theme::handle::ThemeHandle::new(
            app.app_mut(),
            crate::theme::appearance::Appearance::Light,
        );
        let always = ScrollBar::new(
            app.app_mut(),
            theme.clone(),
            "always",
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        let hover = ScrollBar::new(
            app.app_mut(),
            theme,
            "hover",
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default().hover(),
        );
        assert_eq!(always.opacity(app.app()), 1.0);
        assert_eq!(hover.opacity(app.app()), 0.0);
        let _ = app.drain().expect("frame");
    }

    #[test]
    fn modes_are_distinct() {
        assert_ne!(ScrollbarMode::Always, ScrollbarMode::Hover);
    }
}

#[cfg(test)]
mod ui_tests {
    use std::time::Duration;

    use ui_core::{
        event::{InputEvent, Modifiers, PointerButton},
        interaction::{ScrollConfig, ScrollHandle},
        math::Size,
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
    };

    use super::{ScrollBar, ScrollbarAxis, ScrollbarMode, ScrollbarOptions};
    use crate::{
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    const BOX_WIDTH: f32 = 200.0;
    const BOX_HEIGHT: f32 = 100.0;
    const CONTENT_HEIGHT: f32 = 400.0;

    struct ScrollHarness {
        app: TestApp,
        bar: ScrollBar,
        handle: ScrollHandle,
        container: NodeId,
    }

    fn harness(mode: ScrollbarMode) -> ScrollHarness {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let root = app.app().window(window).expect("window").tree().root();
        let container = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("scroll-container")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(BOX_WIDTH)
                        .height(BOX_HEIGHT)
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("container")
        };
        let handle = app
            .app_mut()
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scrollable");
        app.app_mut()
            .set_scroll_content(&handle, Size::new(BOX_WIDTH, CONTENT_HEIGHT))
            .expect("content");
        let options = ScrollbarOptions {
            mode,
            ..ScrollbarOptions::default()
        };
        let bar = ScrollBar::new(
            app.app_mut(),
            theme,
            "bar",
            ScrollbarAxis::Vertical,
            options,
        );
        {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            bar.build(tree, container).expect("scrollbar");
        }
        bar.attach(app.app_mut(), window, container);
        let mut harness = ScrollHarness {
            app,
            bar,
            handle,
            container,
        };
        settle(&mut harness);
        harness
    }

    fn settle(harness: &mut ScrollHarness) {
        for _ in 0..4 {
            harness.app.advance(Duration::from_millis(100));
            let _ = harness.app.drain().expect("frame");
            let _ = harness.bar.sync(harness.app.app_mut()).expect("sync");
        }
        let _ = harness.app.drain().expect("frame");
    }

    fn thumb_bounds(harness: &ScrollHarness) -> ui_core::math::Rect {
        let track = harness.bar.nodes().0.expect("track node");
        let track_bounds = harness.app.probe().bounds(track).expect("track bounds");
        let rect = harness
            .bar
            .thumb_rect(harness.app.app())
            .expect("thumb rect");
        ui_core::math::Rect::new(
            track_bounds.origin.x + rect.origin.x,
            track_bounds.origin.y + rect.origin.y,
            rect.size.width,
            rect.size.height,
        )
    }

    fn scroll_to(harness: &mut ScrollHarness, offset: f32) {
        harness
            .app
            .app_mut()
            .scroll_to(&harness.handle, ui_core::math::Point::new(0.0, offset))
            .expect("scroll");
        let _ = harness.app.drain().expect("frame");
        let _ = harness.bar.sync(harness.app.app_mut()).expect("sync");
        let _ = harness.app.drain().expect("frame");
    }

    #[test]
    fn the_thumb_geometry_follows_the_scroll_state() {
        let mut harness = harness(ScrollbarMode::Always);
        let rect = harness
            .bar
            .thumb_rect(harness.app.app())
            .expect("thumb rect");
        // 轨道高 100、视口 100、内容 400 ⇒ 滑块 25；顶部对齐。
        assert!((rect.size.height - 25.0).abs() < 0.5, "{rect:?}");
        assert_eq!(rect.origin.y, 0.0);
        assert!(harness.bar.scrollable(harness.app.app()));

        scroll_to(&mut harness, 150.0);
        let rect = harness
            .bar
            .thumb_rect(harness.app.app())
            .expect("thumb rect");
        // 可拖动 75、范围 300 ⇒ 偏移 150 对应 37.5。
        assert!((rect.origin.y - 37.5).abs() < 0.5, "{rect:?}");
    }

    #[test]
    fn hovering_fades_the_thumb_in_and_out() {
        let mut harness = harness(ScrollbarMode::Hover);
        assert_eq!(harness.bar.opacity(harness.app.app()), 0.0);
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        let inside = ui_core::math::Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(inside, Modifiers::empty()));
        settle(&mut harness);
        assert_eq!(
            harness.bar.opacity(harness.app.app()),
            1.0,
            "悬停后滑块淡入到 1"
        );

        let outside = ui_core::math::Point::new(bounds.origin.x - 60.0, bounds.origin.y);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(outside, Modifiers::empty()));
        settle(&mut harness);
        assert_eq!(
            harness.bar.opacity(harness.app.app()),
            0.0,
            "离开后滑块淡出到 0"
        );
    }

    /// 淡出动画只在**目标变化**时启动：16 ms 采样在规格时长（120 ms）内到达端点，
    /// 动画在飞时 `sync` 不再重复 `animate`（旧实现逐帧重启 ⇒ 128 ms 只到 0.95）。
    #[test]
    fn the_fade_reaches_its_target_within_the_spec_without_restarting() {
        let mut harness = harness(ScrollbarMode::Hover);
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        let inside = ui_core::math::Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(inside, Modifiers::empty()));
        let mut started = false;
        for _ in 0..8 {
            harness.app.advance(Duration::from_millis(16));
            let _ = harness.app.drain().expect("frame");
            let changed = harness.bar.sync(harness.app.app_mut()).expect("sync");
            if started {
                assert!(
                    !changed,
                    "动画在飞、目标未变时不得重复启动（sync 必须是 no-op）"
                );
            } else {
                assert!(changed, "悬停必须启动淡入动画");
                started = true;
            }
        }
        let opacity = harness.bar.opacity(harness.app.app());
        assert!(
            opacity >= 0.999,
            "120 ms 规格内到达 1（128 ms 实际 {opacity}）"
        );

        let outside = ui_core::math::Point::new(bounds.origin.x - 60.0, bounds.origin.y);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(outside, Modifiers::empty()));
        let mut started = false;
        for _ in 0..8 {
            harness.app.advance(Duration::from_millis(16));
            let _ = harness.app.drain().expect("frame");
            let changed = harness.bar.sync(harness.app.app_mut()).expect("sync");
            if started {
                assert!(!changed, "淡出同样只在目标变化时启动");
            } else {
                assert!(changed, "离开必须启动淡出动画");
                started = true;
            }
        }
        let opacity = harness.bar.opacity(harness.app.app());
        assert!(
            opacity <= 0.001,
            "淡出在 120 ms 规格内到达 0（实际 {opacity}）"
        );
    }

    /// 释放作用域回收信号 / 瞬态槽位；在飞淡出动画被引擎同批取消（无死槽位写入）。
    #[test]
    fn releasing_the_scope_reclaims_state_and_cancels_the_fade() {
        let mut harness = harness(ScrollbarMode::Hover);
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        let inside = ui_core::math::Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(inside, Modifiers::empty()));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.bar.sync(harness.app.app_mut()).expect("sync");
        let window = harness.app.window_id();
        let removed = harness
            .app
            .app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .remove(harness.container)
            .expect("remove container");
        assert!(!removed.is_empty());
        let released = harness.bar.release(harness.app.app_mut());
        assert!(
            released >= 4,
            "悬停 / 可滚动 / 淡出不透明度 / 拖拽快照必须被回收（实际 {released}）"
        );
        assert_eq!(
            harness.bar.release(harness.app.app_mut()),
            0,
            "重复释放幂等"
        );
        // 动画槽已被取消：推进时钟 + 出帧不得写已回收的瞬态值。
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("frame");
    }

    #[test]
    fn dragging_the_thumb_scrolls_proportionally() {
        let mut harness = harness(ScrollbarMode::Always);
        let thumb = thumb_bounds(&harness);
        let origin = ui_core::math::Point::new(
            thumb.origin.x + thumb.size.width / 2.0,
            thumb.origin.y + thumb.size.height / 2.0,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            origin,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        // 第一次越过阈值只把拖拽会话转正（派发 `DragPhase::Start`），位移在随后的 Move 里应用。
        let promoted = ui_core::math::Point::new(origin.x, origin.y + 10.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(promoted, Modifiers::empty()));
        let moved = ui_core::math::Point::new(origin.x, origin.y + 30.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(moved, Modifiers::empty()));
        let _ = harness.app.inject(InputEvent::pointer_up(
            moved,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        // 可拖动 75、范围 300 ⇒ 拖 30 对应偏移 120。
        let offset = harness
            .app
            .app()
            .scroll_state(&harness.handle)
            .expect("state")
            .offset
            .y;
        assert!((offset - 120.0).abs() < 0.5, "拖拽换算后的偏移：{offset}");
    }

    #[test]
    fn clicking_the_track_jumps_towards_the_click() {
        let mut harness = harness(ScrollbarMode::Always);
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        // 点轨道下方 3/4 处（滑块只有 25 高，点在滑块之外）。
        let point = ui_core::math::Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height * 0.75,
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
        let offset = harness
            .app
            .app()
            .scroll_state(&harness.handle)
            .expect("state")
            .offset
            .y;
        assert!(offset > 150.0, "点击轨道下部后偏移过半：{offset}");
    }

    /// `release` 必须摘掉轨道 / 滑块 / 拖拽三个 Action handler。
    ///
    /// E8 审查 F1：这三个 handler 只写引擎滚动状态，泄漏后的「重放点击」不会 panic，
    /// 因此必须直接观察 handler 表项 + `InputOutcome::action`（= handler 真的被调用）。
    #[test]
    fn releasing_the_scrollbar_removes_its_three_action_handlers() {
        let mut harness = harness(ScrollbarMode::Always);
        let actions = [
            harness.bar.inner.track_action.clone(),
            harness.bar.inner.thumb_action.clone(),
            harness.bar.inner.drag_action.clone(),
        ];
        for action in &actions {
            assert!(
                harness.app.app().has_action_handler(action),
                "attach 必须注册轨道 / 滑块 / 拖拽 handler"
            );
        }

        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        // 点轨道下部（避开滑块）触发页面跳转；释放前这次点击必须由 track handler 消费。
        let point = ui_core::math::Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height * 0.75,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let click0 = harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert_eq!(
            click0.action,
            Some(actions[0].clone()),
            "释放前点击轨道必须由 track handler 消费"
        );

        assert!(harness.bar.release(harness.app.app_mut()) > 0);
        for action in &actions {
            assert!(
                !harness.app.app().has_action_handler(action),
                "release 必须摘掉滚动条 handler"
            );
        }

        // 不卸载节点直接释放：重放同一次点击不得再命中已摘除的 handler。
        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let click1 = harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert_eq!(
            click1.action, None,
            "释放后重放点击不得再命中滚动条 handler"
        );
    }

    #[test]
    fn a_settled_scrollbar_requests_no_frames() {
        let mut harness = harness(ScrollbarMode::Always);
        let mut idle = false;
        for _ in 0..5 {
            if !harness.app.drain().expect("frame").rendered {
                idle = true;
                break;
            }
        }
        assert!(idle, "静止滚动条不得持续出帧（I3）");
        let _ = harness.container;
    }
}
