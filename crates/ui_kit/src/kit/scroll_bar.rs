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
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
        tree::NodeTree,
    },
    paint::{
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    window::WindowId,
};

use crate::theme::handle::ThemeHandle;

const MIN_THUMB_TRACK_SHARE: f32 = 0.5;

const MIN_THUMB_LENGTH: LogicalPx = px(2.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarAxis {
    Vertical,
    Horizontal,
}

impl ScrollbarAxis {
    fn length(self, size: LogicalSize) -> LogicalPx {
        match self {
            Self::Vertical => size.height,
            Self::Horizontal => size.width,
        }
    }

    fn component(self, point: LogicalPoint) -> LogicalPx {
        match self {
            Self::Vertical => point.y,
            Self::Horizontal => point.x,
        }
    }

    fn point(self, current: LogicalPoint, offset: LogicalPx) -> LogicalPoint {
        match self {
            Self::Vertical => LogicalPoint::new(current.x, offset),
            Self::Horizontal => LogicalPoint::new(offset, current.y),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarMode {
    Always,
    Hover,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarOptions {
    pub thickness: LogicalPx,
    pub corner_radius: LogicalPx,
    pub min_thumb: LogicalPx,
    pub mode: ScrollbarMode,
    pub inset: LogicalPx,
    pub focus_opacity: f32,
    pub tab_stop: bool,
}

impl Default for ScrollbarOptions {
    fn default() -> Self {
        Self {
            thickness: px(4.0),
            corner_radius: px(1.0),
            min_thumb: px(20.0),
            mode: ScrollbarMode::Always,
            inset: LogicalPx::ZERO,
            focus_opacity: 0.0,
            tab_stop: true,
        }
    }
}

impl ScrollbarOptions {
    #[must_use]
    pub fn hover(mut self) -> Self {
        self.mode = ScrollbarMode::Hover;
        self
    }

    #[must_use]
    pub fn thickness(mut self, thickness: LogicalPx) -> Self {
        self.thickness = thickness.max(px(1.0));
        self
    }

    #[must_use]
    pub fn inset(mut self, inset: LogicalPx) -> Self {
        self.inset = inset.max(LogicalPx::ZERO);
        self
    }

    #[must_use]
    pub fn focus_opacity(mut self, opacity: f32) -> Self {
        self.focus_opacity = opacity.clamp(0.0, 1.0);
        self
    }

    #[must_use]
    pub fn tab_stop(mut self, tab_stop: bool) -> Self {
        self.tab_stop = tab_stop;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DragStart {
    start_offset: LogicalPx,
    max: LogicalPx,
    usable: LogicalPx,
}

struct ScrollBarInner {
    key: SmolStr,
    theme: ThemeHandle,
    axis: ScrollbarAxis,
    options: ScrollbarOptions,
    scope: SignalScope,
    hovered: Signal<bool>,
    scrollable: Signal<bool>,
    focused: Signal<bool>,
    fade: Transient<f32>,
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

#[derive(Clone)]
pub struct ScrollBar {
    inner: Rc<ScrollBarInner>,
}

impl ScrollBar {
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
                focused: app.signal_scoped(scope, false),
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

    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let inner = &self.inner;
        let thickness = inner.options.thickness;
        let inset = inner.options.inset;
        let mut track_style = StyleBuilder::new().absolute();
        track_style = match inner.axis {
            ScrollbarAxis::Vertical => track_style
                .top(inset)
                .right(inset)
                .bottom(inset)
                .width(thickness),
            ScrollbarAxis::Horizontal => track_style
                .left(inset)
                .right(inset)
                .bottom(inset)
                .height(thickness),
        };
        let track = NodeBuilder::boxed()
            .id(format_smolstr!("{}:track", inner.key))
            .style(track_style.build())
            .tab_stop(inner.options.tab_stop)
            .on_click(inner.track_action.clone())
            .build(tree, parent)?;
        let initial = match inner.axis {
            ScrollbarAxis::Vertical => LogicalRect::new(
                LogicalPx::ZERO,
                LogicalPx::ZERO,
                thickness,
                inner.options.min_thumb,
            ),
            ScrollbarAxis::Horizontal => LogicalRect::new(
                LogicalPx::ZERO,
                LogicalPx::ZERO,
                inner.options.min_thumb,
                thickness,
            ),
        };
        let theme = inner.theme.clone();
        let hovered = inner.hovered;
        let scrollable = inner.scrollable;
        let fade = inner.fade;
        let thumb = NodeBuilder::boxed()
            .id(format_smolstr!("{}:thumb", inner.key))
            .layout_override(initial)
            .tab_stop(inner.options.tab_stop)
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

    pub fn attach(
        &self,
        app: &mut App,
        window: WindowId,
        scroll_node: NodeId,
    ) -> Result<(), EngineError> {
        let handle = app
            .scroll_handle(window, scroll_node)
            .ok_or(EngineError::NotScrollable { node: scroll_node })?;
        let track = self
            .inner
            .geometry
            .borrow()
            .track
            .ok_or(EngineError::StaleNode { node: scroll_node })?;
        app.set_scroll_proxy(window, track, Some(scroll_node))?;
        {
            let mut geometry = self.inner.geometry.borrow_mut();
            geometry.window = Some(window);
            geometry.handle = Some(handle);
        }
        let track = self.clone();
        app.on_action(self.inner.track_action.clone(), move |ctx| {
            track.on_track_click(ctx);
        });
        app.on_action(self.inner.thumb_action.clone(), move |_ctx| {});
        let drag = self.clone();
        app.on_action(self.inner.drag_action.clone(), move |ctx| {
            drag.on_thumb_drag(ctx);
        });
        Ok(())
    }

    pub fn release(&self, app: &mut App) -> usize {
        let (window, track) = {
            let geometry = self.inner.geometry.borrow();
            (geometry.window, geometry.track)
        };
        if let (Some(window), Some(track)) = (window, track) {
            detach_proxy(app, window, track);
        }
        app.off_action(&self.inner.track_action);
        app.off_action(&self.inner.thumb_action);
        app.off_action(&self.inner.drag_action);
        app.release_scope(self.inner.scope)
    }

    #[must_use]
    pub fn nodes(&self) -> (Option<NodeId>, Option<NodeId>) {
        let geometry = self.inner.geometry.borrow();
        (geometry.track, geometry.thumb)
    }

    #[must_use]
    pub fn thumb_rect(&self, app: &App) -> Option<LogicalRect> {
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

    #[must_use]
    pub fn opacity(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.fade)
    }

    #[must_use]
    pub fn opacity_transient(&self) -> Transient<f32> {
        self.inner.fade
    }

    #[must_use]
    pub fn scrollable(&self, app: &App) -> bool {
        app.read_with(&self.inner.scrollable, |scrollable| *scrollable)
    }

    pub fn set_focused(&self, app: &mut App, focused: bool) -> bool {
        app.write(&self.inner.focused, focused)
    }

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
        let max = (content - viewport).max(LogicalPx::ZERO);
        let scrollable = max > px(0.5);
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
        let focused = app.read_with(&self.inner.focused, |focused| *focused);
        let target_opacity = match self.inner.options.mode {
            ScrollbarMode::Always => f32::from(scrollable),
            ScrollbarMode::Hover => {
                if !scrollable {
                    0.0
                } else if hovered || dragging {
                    1.0
                } else if focused {
                    self.inner.options.focus_opacity
                } else {
                    0.0
                }
            }
        };
        let current_opacity = *app.read_transient(&self.inner.fade);
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
                .unwrap_or(LogicalPx::ZERO);
            (state, track_len)
        };
        let viewport = self.inner.axis.length(state.viewport);
        let content = self.inner.axis.length(state.content);
        let max = (content - viewport).max(LogicalPx::ZERO);
        let thumb_len = thumb_length(track_len, viewport, content, self.inner.options);
        let click = self.inner.axis.component(hit.local_point);
        let target_offset = track_target_offset(track_len, thumb_len, max, click);
        let _ = ctx
            .app_mut()
            .scroll_to(&handle, self.inner.axis.point(state.offset, target_offset));
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
                    .unwrap_or(LogicalPx::ZERO);
                let viewport = self.inner.axis.length(state.viewport);
                let content = self.inner.axis.length(state.content);
                let max = (content - viewport).max(LogicalPx::ZERO);
                let thumb_len = thumb_length(track_len, viewport, content, self.inner.options);
                let usable = (track_len - thumb_len).max(LogicalPx::ZERO);
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
                let current = ctx
                    .app_mut()
                    .scroll_state(&handle)
                    .map_or(LogicalPoint::ZERO, |state| state.offset);
                let _ = ctx
                    .app_mut()
                    .scroll_to(&handle, self.inner.axis.point(current, target));
            }
            DragPhase::End | DragPhase::Cancel => {
                ctx.app_mut().write_transient(&self.inner.drag, None);
                let _ = ctx.app_mut().release_pointer(window);
            }
        }
    }
}

pub(crate) fn detach_proxy(app: &mut App, window: WindowId, node: NodeId) {
    if let Err(error) = app.set_scroll_proxy(window, node, None) {
        debug_assert!(
            matches!(
                error,
                EngineError::StaleNode { .. } | EngineError::UnknownWindow { .. }
            ),
            "detaching a scroll proxy must tolerate unloaded nodes: {error}"
        );
    }
}

fn thumb_length(
    track_len: LogicalPx,
    viewport: LogicalPx,
    content: LogicalPx,
    options: ScrollbarOptions,
) -> LogicalPx {
    if track_len <= LogicalPx::ZERO || content <= LogicalPx::ZERO {
        return LogicalPx::ZERO;
    }
    let ratio = (viewport / content).clamp(0.0, 1.0);
    let min_thumb = (track_len * MIN_THUMB_TRACK_SHARE)
        .max(MIN_THUMB_LENGTH)
        .min(options.min_thumb)
        .min(track_len);
    (track_len * ratio).max(min_thumb).min(track_len)
}

fn thumb_offset(
    track_len: LogicalPx,
    thumb_len: LogicalPx,
    offset: LogicalPx,
    max: LogicalPx,
) -> LogicalPx {
    let usable = (track_len - thumb_len).max(LogicalPx::ZERO);
    if max <= LogicalPx::ZERO || usable <= LogicalPx::ZERO {
        return LogicalPx::ZERO;
    }
    (usable * (offset.clamp(LogicalPx::ZERO, max) / max)).clamp(LogicalPx::ZERO, usable)
}

fn thumb_rect(
    axis: ScrollbarAxis,
    track_len: LogicalPx,
    options: ScrollbarOptions,
    viewport: LogicalPx,
    content: LogicalPx,
    offset: LogicalPx,
) -> LogicalRect {
    let max = (content - viewport).max(LogicalPx::ZERO);
    let len = thumb_length(track_len, viewport, content, options);
    let pos = thumb_offset(track_len, len, offset, max);
    match axis {
        ScrollbarAxis::Vertical => LogicalRect::new(LogicalPx::ZERO, pos, options.thickness, len),
        ScrollbarAxis::Horizontal => LogicalRect::new(pos, LogicalPx::ZERO, len, options.thickness),
    }
}

fn track_target_offset(
    track_len: LogicalPx,
    thumb_len: LogicalPx,
    max: LogicalPx,
    click: LogicalPx,
) -> LogicalPx {
    let usable = (track_len - thumb_len).max(LogicalPx::ZERO);
    if usable <= LogicalPx::ZERO || max <= LogicalPx::ZERO {
        return LogicalPx::ZERO;
    }
    max * ((click - thumb_len / 2.0) / usable).clamp(0.0, 1.0)
}

fn drag_offset(start: LogicalPx, delta: LogicalPx, usable: LogicalPx, max: LogicalPx) -> LogicalPx {
    if max <= LogicalPx::ZERO {
        return start.max(LogicalPx::ZERO);
    }
    if usable <= LogicalPx::ZERO {
        return start.clamp(LogicalPx::ZERO, max);
    }
    (start + max * (delta / usable)).clamp(LogicalPx::ZERO, max)
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
    use ui_core::math::{LogicalPoint, LogicalPx, LogicalRect, px};

    use super::{
        ScrollBar, ScrollbarAxis, ScrollbarMode, ScrollbarOptions, drag_offset, thumb_length,
        thumb_offset, thumb_rect, track_target_offset,
    };

    fn options() -> ScrollbarOptions {
        ScrollbarOptions::default()
    }

    #[test]
    fn thumb_length_follows_the_visible_fraction_and_clamps() {
        assert_eq!(
            thumb_length(px(200.0), px(100.0), px(400.0), options()),
            px(50.0)
        );
        assert_eq!(
            thumb_length(px(200.0), px(10.0), px(10_000.0), options()),
            px(20.0),
            "极长内容夹到最小长度"
        );
        assert_eq!(
            thumb_length(px(10.0), px(100.0), px(100.0), options()),
            px(10.0),
            "内容不足一屏 ⇒ 滑块占满轨道"
        );
        assert_eq!(
            thumb_length(LogicalPx::ZERO, px(100.0), px(400.0), options()),
            LogicalPx::ZERO
        );
    }

    #[test]
    fn a_short_track_scales_the_minimum_thumb_down() {
        let short = thumb_length(px(16.0), px(16.0), px(10_000.0), options());
        assert_eq!(short, px(8.0), "16px 轨道上的最小滑块按轨道比例收缩");
        assert_eq!(
            thumb_offset(px(16.0), short, px(9_984.0), px(9_984.0)),
            px(8.0),
            "极短轨道仍留下可拖拽行程"
        );
        assert_eq!(
            thumb_length(px(4.0), px(1.0), px(10_000.0), options()),
            px(2.0),
            "极端轨道仍保留 2px 最小滑块"
        );
        assert_eq!(
            thumb_length(px(200.0), px(10.0), px(10_000.0), options()),
            px(20.0),
            "正常轨道仍使用配置下限"
        );
    }

    #[test]
    fn thumb_offset_maps_the_scroll_range_onto_the_track() {
        assert_eq!(
            thumb_offset(px(200.0), px(50.0), LogicalPx::ZERO, px(300.0)),
            LogicalPx::ZERO
        );
        assert_eq!(
            thumb_offset(px(200.0), px(50.0), px(150.0), px(300.0)),
            px(75.0)
        );
        assert_eq!(
            thumb_offset(px(200.0), px(50.0), px(300.0), px(300.0)),
            px(150.0)
        );
        assert_eq!(
            thumb_offset(px(200.0), px(50.0), px(10_000.0), px(300.0)),
            px(150.0),
            "越界 clamp"
        );
        assert_eq!(
            thumb_offset(px(200.0), px(200.0), px(10.0), LogicalPx::ZERO),
            LogicalPx::ZERO,
            "不可滚动"
        );
    }

    #[test]
    fn thumb_rect_covers_both_axes() {
        assert_eq!(
            thumb_rect(
                ScrollbarAxis::Vertical,
                px(200.0),
                options(),
                px(100.0),
                px(400.0),
                px(200.0)
            ),
            LogicalRect::new(px(0.0), px(100.0), px(4.0), px(50.0))
        );
        assert_eq!(
            thumb_rect(
                ScrollbarAxis::Horizontal,
                px(200.0),
                options(),
                px(100.0),
                px(400.0),
                LogicalPx::ZERO
            ),
            LogicalRect::new(px(0.0), px(0.0), px(50.0), px(4.0))
        );
    }

    #[test]
    fn point_preserves_the_other_axis() {
        let current = LogicalPoint::new(px(30.0), px(40.0));
        assert_eq!(
            ScrollbarAxis::Vertical.point(current, px(90.0)),
            LogicalPoint::new(px(30.0), px(90.0)),
            "垂直条只改 y，保留双轴容器的 x"
        );
        assert_eq!(
            ScrollbarAxis::Horizontal.point(current, px(90.0)),
            LogicalPoint::new(px(90.0), px(40.0)),
            "水平条只改 x，保留双轴容器的 y"
        );
    }

    #[test]
    fn track_click_centers_the_thumb() {
        assert_eq!(
            track_target_offset(px(200.0), px(50.0), px(300.0), px(100.0)),
            px(150.0)
        );
        assert_eq!(
            track_target_offset(px(200.0), px(50.0), px(300.0), LogicalPx::ZERO),
            LogicalPx::ZERO
        );
        assert_eq!(
            track_target_offset(px(200.0), px(50.0), px(300.0), px(200.0)),
            px(300.0)
        );
        assert_eq!(
            track_target_offset(px(200.0), px(200.0), px(300.0), px(100.0)),
            LogicalPx::ZERO
        );
    }

    #[test]
    fn drag_moves_by_the_track_to_content_ratio() {
        assert_eq!(
            drag_offset(LogicalPx::ZERO, px(75.0), px(150.0), px(300.0)),
            px(150.0)
        );
        assert_eq!(
            drag_offset(px(100.0), px(-150.0), px(150.0), px(300.0)),
            LogicalPx::ZERO
        );
        assert_eq!(
            drag_offset(px(100.0), px(10_000.0), px(150.0), px(300.0)),
            px(300.0)
        );
        assert_eq!(
            drag_offset(px(42.0), px(5.0), LogicalPx::ZERO, LogicalPx::ZERO),
            px(42.0),
            "不可拖动时保持"
        );
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
        error::EngineError,
        event::{Action, InputEvent, Modifiers, PointerButton},
        interaction::{ScrollAxes, ScrollConfig, ScrollHandle},
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{style::StylePatch, style_builder::StyleBuilder},
        test_support::app::TestApp,
    };

    use super::{ScrollBar, ScrollbarAxis, ScrollbarMode, ScrollbarOptions};
    use crate::theme::{appearance::Appearance, handle::ThemeHandle};

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
        harness_with(mode, BOX_HEIGHT, CONTENT_HEIGHT)
    }

    fn harness_with(mode: ScrollbarMode, box_height: f32, content_height: f32) -> ScrollHarness {
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
                        .width(px(BOX_WIDTH))
                        .height(px(box_height))
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
            .set_scroll_content(&handle, LogicalSize::new(px(BOX_WIDTH), px(content_height)))
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
        bar.attach(app.app_mut(), window, container)
            .expect("scrollbar attach");
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

    fn thumb_bounds(harness: &ScrollHarness) -> ui_core::math::LogicalRect {
        let track = harness.bar.nodes().0.expect("track node");
        let track_bounds = harness.app.probe().bounds(track).expect("track bounds");
        let rect = harness
            .bar
            .thumb_rect(harness.app.app())
            .expect("thumb rect");
        ui_core::math::LogicalRect::new(
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
            .scroll_to(
                &harness.handle,
                ui_core::math::LogicalPoint::new(px(0.0), px(offset)),
            )
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
        assert!((rect.size.height - 25.0).abs() < px(0.5), "{rect:?}");
        assert_eq!(rect.origin.y, px(0.0));
        assert!(harness.bar.scrollable(harness.app.app()));

        scroll_to(&mut harness, 150.0);
        let rect = harness
            .bar
            .thumb_rect(harness.app.app())
            .expect("thumb rect");
        assert!((rect.origin.y - 37.5).abs() < px(0.5), "{rect:?}");
    }

    #[test]
    fn hovering_fades_the_thumb_in_and_out() {
        let mut harness = harness(ScrollbarMode::Hover);
        assert_eq!(harness.bar.opacity(harness.app.app()), 0.0);
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        let inside = ui_core::math::LogicalPoint::new(
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

        let outside = ui_core::math::LogicalPoint::new(bounds.origin.x - 60.0, bounds.origin.y);
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

    #[test]
    fn the_fade_reaches_its_target_within_the_spec_without_restarting() {
        let mut harness = harness(ScrollbarMode::Hover);
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        let inside = ui_core::math::LogicalPoint::new(
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

        let outside = ui_core::math::LogicalPoint::new(bounds.origin.x - 60.0, bounds.origin.y);
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

    #[test]
    fn releasing_the_scope_reclaims_state_and_cancels_the_fade() {
        let mut harness = harness(ScrollbarMode::Hover);
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        let inside = ui_core::math::LogicalPoint::new(
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
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("frame");
    }

    #[test]
    fn dragging_the_thumb_scrolls_proportionally() {
        let mut harness = harness(ScrollbarMode::Always);
        let thumb = thumb_bounds(&harness);
        let origin = ui_core::math::LogicalPoint::new(
            thumb.origin.x + thumb.size.width / 2.0,
            thumb.origin.y + thumb.size.height / 2.0,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            origin,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let promoted = ui_core::math::LogicalPoint::new(origin.x, origin.y + 10.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(promoted, Modifiers::empty()));
        let moved = ui_core::math::LogicalPoint::new(origin.x, origin.y + 30.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(moved, Modifiers::empty()));
        let _ = harness.app.inject(InputEvent::pointer_up(
            moved,
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
        assert!(
            (offset - 120.0).abs() < px(0.5),
            "拖拽换算后的偏移：{offset}"
        );
    }

    #[test]
    fn a_very_short_track_keeps_a_draggable_thumb() {
        let mut harness = harness_with(ScrollbarMode::Always, 16.0, 40_000.0);
        let track = harness.bar.nodes().0.expect("track node");
        let track_bounds = harness.app.probe().bounds(track).expect("track bounds");
        assert!(
            (track_bounds.size.height.value() - 16.0).abs() < 0.5,
            "前置：16px 极短轨道，实际 {}",
            track_bounds.size.height.value()
        );
        let thumb = thumb_bounds(&harness);
        assert!(
            thumb.size.height >= px(2.0),
            "极短轨道的最小滑块不得塌成零高度：{}",
            thumb.size.height.value()
        );
        assert!(
            track_bounds.size.height - thumb.size.height >= px(2.0),
            "极短轨道必须留下拖拽行程：track {} thumb {}",
            track_bounds.size.height.value(),
            thumb.size.height.value()
        );

        let origin = ui_core::math::LogicalPoint::new(
            thumb.origin.x + thumb.size.width / 2.0,
            thumb.origin.y + thumb.size.height / 2.0,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            origin,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let promoted = ui_core::math::LogicalPoint::new(origin.x, origin.y + 10.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(promoted, Modifiers::empty()));
        let moved = ui_core::math::LogicalPoint::new(origin.x, origin.y + 20.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(moved, Modifiers::empty()));
        let _ = harness.app.inject(InputEvent::pointer_up(
            moved,
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
        assert!(
            offset > px(0.0),
            "拖拽极短轨道的滑块必须改变滚动偏移：{offset}"
        );
    }

    #[test]
    fn clicking_the_track_jumps_towards_the_click() {
        let mut harness = harness(ScrollbarMode::Always);
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        let point = ui_core::math::LogicalPoint::new(
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
        assert!(offset > px(150.0), "点击轨道下部后偏移过半：{offset}");
    }

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
        let point = ui_core::math::LogicalPoint::new(
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

    struct BothAxesHarness {
        app: TestApp,
        vertical: ScrollBar,
        horizontal: ScrollBar,
        handle: ScrollHandle,
    }

    fn both_axes_harness() -> BothAxesHarness {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let root = app.app().window(window).expect("window").tree().root();
        let outer = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("both-axes-outer")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .size(px(BOX_WIDTH), px(BOX_HEIGHT))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("outer")
        };
        let container = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("both-axes-viewport")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .size_full()
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, outer)
                .expect("container")
        };
        let handle = app
            .app_mut()
            .make_scroll(
                window,
                container,
                ScrollConfig {
                    axes: ScrollAxes::Both,
                    line_height: px(48.0),
                },
            )
            .expect("双轴滚动容器");
        app.app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(400.0), px(400.0)))
            .expect("内容尺寸");
        let options = ScrollbarOptions::default();
        let vertical = ScrollBar::new(
            app.app_mut(),
            theme.clone(),
            "vertical-scrollbar",
            ScrollbarAxis::Vertical,
            options,
        );
        let horizontal = ScrollBar::new(
            app.app_mut(),
            theme,
            "horizontal-scrollbar",
            ScrollbarAxis::Horizontal,
            options,
        );
        {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            vertical.build(tree, outer).expect("垂直滚动条");
            horizontal.build(tree, outer).expect("水平滚动条");
        }
        vertical
            .attach(app.app_mut(), window, container)
            .expect("vertical attach");
        horizontal
            .attach(app.app_mut(), window, container)
            .expect("horizontal attach");
        let mut harness = BothAxesHarness {
            app,
            vertical,
            horizontal,
            handle,
        };
        for _ in 0..4 {
            harness.app.advance(Duration::from_millis(100));
            let _ = harness.app.drain().expect("frame");
            let _ = harness.vertical.sync(harness.app.app_mut()).expect("sync");
            let _ = harness
                .horizontal
                .sync(harness.app.app_mut())
                .expect("sync");
        }
        let _ = harness.app.drain().expect("frame");
        harness
    }

    impl BothAxesHarness {
        fn offset(&self) -> LogicalPoint {
            self.app
                .app()
                .scroll_state(&self.handle)
                .expect("滚动状态")
                .offset
        }

        fn track_bounds(&self, bar: &ScrollBar) -> LogicalRect {
            let (Some(track), _) = bar.nodes() else {
                panic!("滚动条必须已构建");
            };
            self.app.probe().bounds(track).expect("轨道 bounds")
        }

        fn click(&mut self, point: LogicalPoint) -> Option<Action> {
            let _ = self.app.inject(InputEvent::pointer_down(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            self.app
                .inject(InputEvent::pointer_up(
                    point,
                    PointerButton::Primary,
                    Modifiers::empty(),
                ))
                .action
        }

        fn sync_bars(&mut self) {
            for _ in 0..2 {
                let _ = self.vertical.sync(self.app.app_mut()).expect("sync");
                let _ = self.horizontal.sync(self.app.app_mut()).expect("sync");
                let _ = self.app.drain().expect("frame");
            }
        }

        fn thumb_center(&self, bar: &ScrollBar) -> LogicalPoint {
            let track = self.track_bounds(bar);
            let thumb = bar.thumb_rect(self.app.app()).expect("滑块 rect");
            LogicalPoint::new(
                track.origin.x + thumb.origin.x + thumb.size.width / 2.0,
                track.origin.y + thumb.origin.y + thumb.size.height / 2.0,
            )
        }

        fn drag_thumb(&mut self, origin: LogicalPoint, delta: LogicalPoint) -> Option<Action> {
            let _ = self.app.inject(InputEvent::pointer_down(
                origin,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let promoted = LogicalPoint::new(origin.x + delta.x / 2.0, origin.y + delta.y / 2.0);
            let started = self
                .app
                .inject(InputEvent::pointer_move(promoted, Modifiers::empty()))
                .action;
            let moved = LogicalPoint::new(origin.x + delta.x, origin.y + delta.y);
            let _ = self
                .app
                .inject(InputEvent::pointer_move(moved, Modifiers::empty()));
            let _ = self.app.inject(InputEvent::pointer_up(
                moved,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            started
        }
    }

    #[test]
    fn dual_axis_scrollbar_preserves_other_axis() {
        let mut harness = both_axes_harness();
        let baseline = LogicalPoint::new(px(50.0), px(60.0));
        harness
            .app
            .app_mut()
            .scroll_to(&harness.handle, baseline)
            .expect("基线滚动");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(harness.offset(), baseline, "前置：双轴基线偏移");

        let vertical_track = harness.track_bounds(&harness.vertical);
        let vertical_point = LogicalPoint::new(
            vertical_track.origin.x + vertical_track.size.width / 2.0,
            vertical_track.origin.y + vertical_track.size.height * 0.75,
        );
        let vertical_action = harness.vertical.inner.track_action.clone();
        assert_eq!(
            harness.click(vertical_point),
            Some(vertical_action),
            "点击点必须命中垂直条轨道"
        );
        let after_vertical = harness.offset();
        assert!(
            after_vertical.y > baseline.y,
            "垂直条轨道点击必须改变 y：{after_vertical:?}"
        );
        assert_eq!(
            after_vertical.x, baseline.x,
            "垂直条不得清掉横向偏移：{after_vertical:?}"
        );

        let horizontal_track = harness.track_bounds(&harness.horizontal);
        let horizontal_point = LogicalPoint::new(
            horizontal_track.origin.x + horizontal_track.size.width * 0.75,
            horizontal_track.origin.y + horizontal_track.size.height / 2.0,
        );
        let horizontal_action = harness.horizontal.inner.track_action.clone();
        assert_eq!(
            harness.click(horizontal_point),
            Some(horizontal_action),
            "点击点必须命中水平条轨道"
        );
        let after_horizontal = harness.offset();
        assert!(
            after_horizontal.x > after_vertical.x,
            "水平条轨道点击必须改变 x：{after_horizontal:?}"
        );
        assert_eq!(
            after_horizontal.y, after_vertical.y,
            "水平条不得清掉纵向偏移：{after_horizontal:?}"
        );
    }

    #[test]
    fn dual_axis_scrollbar_thumb_drag_preserves_other_axis() {
        let mut harness = both_axes_harness();
        let baseline = LogicalPoint::new(px(50.0), px(60.0));
        harness
            .app
            .app_mut()
            .scroll_to(&harness.handle, baseline)
            .expect("基线滚动");
        harness.sync_bars();
        assert_eq!(harness.offset(), baseline, "前置：双轴基线偏移");

        let vertical_thumb = harness.thumb_center(&harness.vertical);
        let vertical_drag = harness.vertical.inner.drag_action.clone();
        assert_eq!(
            harness.drag_thumb(vertical_thumb, LogicalPoint::new(px(0.0), px(20.0))),
            Some(vertical_drag),
            "垂直滑块向下拖动必须由垂直条 drag handler 消费"
        );
        let after_vertical = harness.offset();
        assert!(
            after_vertical.y > baseline.y,
            "垂直滑块向下拖动必须增加 y：{after_vertical:?}"
        );
        assert_eq!(
            after_vertical.x, baseline.x,
            "垂直滑块拖动不得清掉横向偏移：{after_vertical:?}"
        );

        harness.sync_bars();
        let horizontal_thumb = harness.thumb_center(&harness.horizontal);
        let horizontal_drag = harness.horizontal.inner.drag_action.clone();
        assert_eq!(
            harness.drag_thumb(horizontal_thumb, LogicalPoint::new(px(20.0), px(0.0))),
            Some(horizontal_drag),
            "水平滑块向右拖动必须由水平条 drag handler 消费"
        );
        let after_horizontal = harness.offset();
        assert!(
            after_horizontal.x > after_vertical.x,
            "水平滑块向右拖动必须增加 x：{after_horizontal:?}"
        );
        assert_eq!(
            after_horizontal.y, after_vertical.y,
            "水平滑块拖动不得清掉纵向偏移：{after_horizontal:?}"
        );
    }

    struct PageBarHarness {
        app: TestApp,
        bar: ScrollBar,
        page: ScrollHandle,
        inner: ScrollHandle,
    }

    enum TargetState {
        Active,
        Hidden,
        Disabled,
    }

    fn page_bar_harness(state: TargetState) -> PageBarHarness {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let root = app.app().window(window).expect("window").tree().root();
        let page = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(px(360.0))
                        .height(px(240.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("page")
        };
        let page_handle = app
            .app_mut()
            .make_scroll(window, page, ScrollConfig::default())
            .expect("page scroll");
        app.app_mut()
            .set_scroll_content(&page_handle, LogicalSize::new(px(360.0), px(2_000.0)))
            .expect("page content");
        let inner = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            let builder = NodeBuilder::boxed().id("inner").style(
                StyleBuilder::new()
                    .relative()
                    .width(px(200.0))
                    .height(px(120.0))
                    .overflow_hidden()
                    .build(),
            );
            let builder = match state {
                TargetState::Active => builder,
                TargetState::Hidden => builder.bind_style(|_| StylePatch {
                    visible: Some(false),
                    ..StylePatch::default()
                }),
                TargetState::Disabled => builder.bind_style(|_| StylePatch {
                    interactive: Some(false),
                    ..StylePatch::default()
                }),
            };
            builder.build(tree, page).expect("inner")
        };
        let inner_handle = app
            .app_mut()
            .make_scroll(window, inner, ScrollConfig::default())
            .expect("inner scroll");
        app.app_mut()
            .set_scroll_content(&inner_handle, LogicalSize::new(px(200.0), px(400.0)))
            .expect("inner content");
        let bar = ScrollBar::new(
            app.app_mut(),
            theme,
            "page-bar",
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            bar.build(tree, page).expect("bar");
        }
        bar.attach(app.app_mut(), window, inner)
            .expect("bar attach");
        let mut harness = PageBarHarness {
            app,
            bar,
            page: page_handle,
            inner: inner_handle,
        };
        settle_page_bar(&mut harness);
        harness
    }

    fn settle_page_bar(harness: &mut PageBarHarness) {
        for _ in 0..4 {
            let _ = harness.app.drain().expect("frame");
            let _ = harness.bar.sync(harness.app.app_mut()).expect("sync");
        }
        let _ = harness.app.drain().expect("frame");
    }

    fn wheel_point(harness: &PageBarHarness) -> LogicalPoint {
        let (track, _) = harness.bar.nodes();
        let track = track.expect("track");
        let bounds = harness.app.probe().bounds(track).expect("track bounds");
        LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn wheel(harness: &mut PageBarHarness, point: LogicalPoint) {
        let _ = harness.app.inject(InputEvent::scroll_lines(
            point,
            LogicalPoint::new(px(0.0), px(-1.0)),
            Modifiers::empty(),
        ));
    }

    fn offsets(harness: &PageBarHarness) -> (LogicalPoint, LogicalPoint) {
        let inner = harness
            .app
            .app()
            .scroll_state(&harness.inner)
            .expect("inner state")
            .offset;
        let page = harness
            .app
            .app()
            .scroll_state(&harness.page)
            .expect("page state")
            .offset;
        (inner, page)
    }

    #[test]
    fn scrollbar_release_clears_the_proxy_and_is_safe() {
        let mut harness = page_bar_harness(TargetState::Active);
        let point = wheel_point(&harness);
        assert_eq!(
            harness.app.probe().hit(point).map(|hit| hit.node),
            harness.bar.nodes().0,
            "滚轮落点必须命中轨道"
        );
        wheel(&mut harness, point);
        let (inner, page) = offsets(&harness);
        assert!(inner.y > px(0.0), "代理生效：滚轮滚内层");
        assert_eq!(page, LogicalPoint::ZERO, "不得滚外层页面");

        let released = harness.bar.release(harness.app.app_mut());
        assert!(released > 0, "释放回收滚动条状态");
        assert_eq!(
            harness.bar.release(harness.app.app_mut()),
            0,
            "重复释放幂等"
        );
        harness.app.advance(Duration::from_millis(300));
        wheel(&mut harness, point);
        let (inner_after, page_after) = offsets(&harness);
        assert_eq!(inner_after, inner, "释放后内层不再被滚动条代理");
        assert!(
            page_after.y > page.y,
            "释放后滚轮继续落到外层页面：{page:?} -> {page_after:?}"
        );
        let window = harness.app.window_id();
        harness
            .app
            .app_mut()
            .remove_node(window, harness.bar.nodes().0.expect("track"))
            .expect("remove track");
        assert_eq!(
            harness.bar.release(harness.app.app_mut()),
            0,
            "节点卸载后释放不 panic"
        );
        let _ = harness.app.drain().expect("frame");
    }

    #[test]
    fn proxy_target_that_is_hidden_or_disabled_is_skipped() {
        for state in [TargetState::Hidden, TargetState::Disabled] {
            let mut harness = page_bar_harness(state);
            let point = wheel_point(&harness);
            wheel(&mut harness, point);
            let (inner, page) = offsets(&harness);
            assert_eq!(inner, LogicalPoint::ZERO, "不可见/禁用的目标不得滚动");
            assert!(page.y > px(0.0), "候选被跳过后继续向外层页面滚动：{page:?}");
        }
    }

    #[test]
    fn attach_validates_the_target_before_writing_its_geometry() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let root = app.app().window(window).expect("window").tree().root();
        let container = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("proxy-target")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(px(200.0))
                        .height(px(100.0))
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
            .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
            .expect("content");
        let bare = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("bare")
                .style(StyleBuilder::new().width(px(40.0)).height(px(40.0)).build())
                .build(tree, root)
                .expect("bare")
        };
        let bar = ScrollBar::new(
            app.app_mut(),
            theme,
            "validated",
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        let error = bar
            .attach(app.app_mut(), window, container)
            .expect_err("未构建的滚动条不得 attach");
        assert!(
            matches!(error, EngineError::StaleNode { node } if node == container),
            "未构建必须报 StaleNode(target)：{error:?}"
        );
        assert_eq!(bar.nodes(), (None, None), "失败不得写 geometry");
        {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            bar.build(tree, container).expect("scrollbar");
        }
        let track = bar.nodes().0.expect("track");
        let error = bar
            .attach(app.app_mut(), window, bare)
            .expect_err("非滚动目标不得 attach");
        assert!(
            matches!(error, EngineError::NotScrollable { node } if node == bare),
            "非滚动目标必须报 NotScrollable：{error:?}"
        );
        assert!(
            bar.thumb_rect(app.app()).is_none(),
            "代理失败不得半写 geometry"
        );
        {
            let tree = app.app().window(window).expect("window").tree();
            assert!(
                tree.scroll_proxy(track).is_none(),
                "代理失败不得注册 track 代理"
            );
        }
        bar.attach(app.app_mut(), window, container)
            .expect("合法目标 attach");
        {
            let tree = app.app().window(window).expect("window").tree();
            assert_eq!(
                tree.scroll_proxy(track),
                Some(container),
                "attach 成功后 track 代理到滚动目标"
            );
        }
        assert!(bar.thumb_rect(app.app()).is_some());
        assert!(bar.release(app.app_mut()) > 0);
        assert_eq!(bar.release(app.app_mut()), 0, "重复释放幂等");
    }
}
