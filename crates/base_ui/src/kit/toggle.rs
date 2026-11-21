//! 开关 / 复选 / 单选（旧 `crate::toggle` 的引擎化重建，P2-4a）。
//!
//! 旧实现用两路 `icons` 交叉叠化（`geometry_color` + `geometry_opacity` 两条 `transition`），
//! 一个 `on_click` 把 `checked` 取反交给调用方。新引擎**一次只驱动一个目标**（评估 §7.1），所以这里
//! 把「勾选程度」收成**一条** `Transient<f32>`（0 = 未选，1 = 选中）：
//!
//! - 状态机与点击 / Enter / Space 激活、禁用、焦点、a11y 状态都复用 [`Interactive`]（`chrome(false)`
//!   去掉按钮外壳，视觉画在子节点上）；
//! - 勾选动画 `App::animate(mix)` 由 `sync` 在 `checked` 变化时启动；子节点的
//!   `bind_style` 直接按 `mix` 插值（背景 / 边框 / 勾 / 点的透明度 / 滑块的布局偏移）；
//! - 变体：`Checkbox`（方框 + 勾）、`Radio`（圆框 + 内点）、`Switch`（轨道 + 滑块）。旧实现只有
//!   复选 / 单选两种；Switch 是本批按任务要求新增，视觉用主题 `toggle` 配方（登记见 spec-delta）。
//!
//! # 键盘
//!
//! `Space` / `Enter` 走引擎「焦点节点激活」通道（与鼠标点击同一个 Action）；旧实现只有鼠标点击，
//! 这是本批新增的可访问性语义。方向键不消费。
//!
//! # a11y
//!
//! 根节点按变体声明 `CheckBox` / `RadioButton` / `Switch`，`checked` 走 `A11yPatch.checked`
//! （`accesskit::Toggled`；AccessKit 的这三个角色不读 `selected`），标签是可访问名。
//!
//! # 视觉差异（登记）
//!
//! 旧实现是「两个几何图标交叉叠化」，新实现是「方框/圆框填充 + 勾/点渐显」：勾是 E7 图标
//! （`IconKind::Check`），其颜色固定为 `FgOnBrand`；未选中态的方框边缘取 `toggle.enabled.unchecked`
//! 角色。像素与旧实现不同，语义一致；原因见 spec-delta §3。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::SmolStr;
use ui_core::{
    a11y::A11yRole,
    app::App,
    error::EngineError,
    event::Event,
    interaction::ActionCtx,
    layout::LayoutOverride,
    math::Rect,
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
    },
    paint::{
        color::{Color, ColorResolver},
        primitive::Border,
        style::StylePatch,
    },
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        icon::IconKind,
        interactive::{Interactive, mount_dynamic_icon},
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{components::ToggleColors, handle::ThemeHandle, role::Role},
};

/// 开关的形状。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToggleKind {
    /// 复选：方框 + 勾。
    #[default]
    Checkbox,
    /// 单选：圆框 + 内点。
    Radio,
    /// 开关：轨道 + 滑块。
    Switch,
}

/// 开关 / 复选 / 单选。
#[derive(Clone)]
pub struct Toggle {
    inner: Rc<ToggleInner>,
    kind: ToggleKind,
    size: UnifiedSize,
    label: Option<SmolStr>,
    disabled: bool,
    initial_checked: bool,
    callback: Option<Rc<dyn Fn(bool, &mut App)>>,
}

struct ToggleInner {
    theme: ThemeHandle,
    scope: SignalScope,
    interactive: Interactive,
    checked: Signal<bool>,
    mix: Transient<f32>,
    /// 动画目标（只在目标变化时启动动画）。
    mix_target: Cell<f32>,
    geometry: Rc<RefCell<Geometry>>,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    node: Option<NodeId>,
    control: Option<NodeId>,
    knob: Option<NodeId>,
    mark: Option<NodeId>,
}

impl Toggle {
    /// 建开关（默认复选）。`key` 是动作身份与调试 id 前缀。
    pub fn new(app: &mut App, theme: ThemeHandle, key: impl Into<SmolStr>) -> Self {
        let scope = app.signal_scope();
        let checked = app.signal_scoped(scope, false);
        let mix = app.transient_scoped(scope, 0.0f32);
        let interactive = Interactive::new(app, theme.clone(), key)
            .chrome(false)
            .a11y_role(A11yRole::CheckBox)
            .checked_signal(checked);
        Self {
            inner: Rc::new(ToggleInner {
                theme,
                scope,
                interactive,
                checked,
                mix,
                mix_target: Cell::new(0.0),
                geometry: Rc::new(RefCell::new(Geometry::default())),
            }),
            kind: ToggleKind::Checkbox,
            size: UnifiedSize::Medium,
            label: None,
            disabled: false,
            initial_checked: false,
            callback: None,
        }
    }

    /// 复选。
    #[must_use]
    pub fn checkbox(mut self) -> Self {
        self.kind = ToggleKind::Checkbox;
        self
    }

    /// 单选。
    #[must_use]
    pub fn radio(mut self) -> Self {
        self.kind = ToggleKind::Radio;
        self
    }

    /// 开关。
    #[must_use]
    pub fn switch(mut self) -> Self {
        self.kind = ToggleKind::Switch;
        self
    }

    /// 初始勾选态。
    #[must_use]
    pub fn checked(mut self, checked: bool) -> Self {
        self.initial_checked = checked;
        self
    }

    /// 文字标签（同时是可访问名）。
    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 禁用。
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// 勾选变化回调（用户点击 / Enter / Space 触发；程序化 `set_checked` 不触发）。
    #[must_use]
    pub fn on_change(mut self, callback: impl Fn(bool, &mut App) + 'static) -> Self {
        self.callback = Some(Rc::new(callback));
        self
    }

    /// 根节点（`build` 之后可用）。
    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().node
    }

    /// 几何控制节点（方框 / 圆框 / 轨道；测试用）。
    #[must_use]
    pub fn control_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().control
    }

    /// 滑块节点（仅 Switch；测试用）。
    #[must_use]
    pub fn knob_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().knob
    }

    /// 当前勾选态。
    #[must_use]
    pub fn is_checked(&self, app: &App) -> bool {
        app.read_with(&self.inner.checked, |checked| *checked)
    }

    /// 勾选过渡进度（0 = 未选，1 = 选中）。
    #[must_use]
    pub fn mix(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.mix)
    }

    /// 程序化设置勾选态（不触发 `on_change`；动画由 `sync` 推进）。
    pub fn set_checked(&self, app: &mut App, checked: bool) -> bool {
        app.write(&self.inner.checked, checked)
    }

    /// 建节点：交互根 + 几何控制 + 标签，并注册点击 / 键盘激活回调。
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode` / 图标资源错误。
    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let inner = &self.inner;
        app.write(&inner.checked, self.initial_checked);
        app.write_transient(&inner.mix, if self.initial_checked { 1.0 } else { 0.0 });
        inner
            .mix_target
            .set(if self.initial_checked { 1.0 } else { 0.0 });

        let checked = inner.checked;
        let disabled_signal = inner.interactive.disabled_signal();
        let callback = self.callback.clone();
        let interactive = inner
            .interactive
            .clone()
            .with_unified_size(self.size)
            .variant(crate::theme::components::InteractiveVariant::Ghost)
            .disabled(self.disabled)
            .checked_signal(checked)
            .a11y_role(match self.kind {
                ToggleKind::Checkbox => A11yRole::CheckBox,
                ToggleKind::Radio => A11yRole::RadioButton,
                ToggleKind::Switch => A11yRole::Switch,
            });
        let interactive = if let Some(label) = &self.label {
            interactive.label(label.clone())
        } else {
            interactive
        };
        let interactive = interactive.on_click(move |ctx: &mut ActionCtx<'_>| {
            let next = !*ctx.read(&checked);
            ctx.write(&checked, next);
            if let Some(callback) = &callback {
                callback(next, ctx.app_mut());
            }
        });

        let theme = inner.theme.clone();
        let kind = self.kind;
        let size = self.size;
        let label = self.label.clone();
        let mix = inner.mix;
        let geometry = Rc::clone(&inner.geometry);
        let for_label = interactive.clone();
        let watcher = interactive.clone();
        let node = interactive.build(app, window, parent, move |app, window, parent| {
            let control = build_control(
                app,
                window,
                parent,
                kind,
                size,
                theme.clone(),
                mix,
                disabled_signal,
            )?;
            watcher.watch(control.node);
            if let Some(knob) = control.knob {
                watcher.watch(knob);
            }
            if let Some(mark) = control.mark {
                watcher.watch(mark);
            }
            if let Some(label) = label {
                let text_style = TextStyle::new(size.text_size());
                let tree = app
                    .window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut();
                let mut style = StyleBuilder::new().min_width_0().build();
                style.inner.flex_shrink = 1.0;
                let label_node = NodeBuilder::text(label, text_style)
                    .style(style)
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(for_label.foreground_color(cx)),
                        ..StylePatch::default()
                    })
                    .build(tree, parent)?;
                watcher.watch(label_node);
            }
            let mut target = geometry.borrow_mut();
            target.control = Some(control.node);
            target.knob = control.knob;
            target.mark = control.mark;
            Ok(())
        })?;
        {
            let mut geo = inner.geometry.borrow_mut();
            geo.window = Some(window);
            geo.node = Some(node);
        }
        Ok(node)
    }

    /// 帧驱动同步：交互状态 + 勾选动画 + Switch 滑块几何。
    ///
    /// # Errors
    /// 同 [`Interactive::sync`] / `StaleNode`。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.interactive.sync(app)?;
        let checked = self.is_checked(app);
        let target = if checked { 1.0 } else { 0.0 };
        if self.inner.mix_target.get() != target {
            self.inner.mix_target.set(target);
            let Some(window) = self.inner.geometry.borrow().window else {
                return Ok(changed);
            };
            let nodes = self.inner.interactive.animation_nodes();
            app.animate(
                window,
                ui_core::interaction::AnimatedValue::Scalar {
                    target: self.inner.mix,
                    to: target,
                },
                &nodes,
                ui_core::interaction::AnimationSpec {
                    duration: self.inner.theme.tokens().animation_tick,
                    easing: ui_core::interaction::Easing::EaseOutCubic,
                },
            )?;
            changed = true;
        }
        if self.kind == ToggleKind::Switch {
            changed |= self.sync_knob(app)?;
        }
        Ok(changed)
    }

    /// 转发按下 / 抬起事件。
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.inner.interactive.dispatch(app, event)
    }

    /// 运行时禁用。
    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        self.inner.interactive.set_disabled(app, disabled)
    }

    /// 释放信号作用域与 Action handler。
    pub fn release(&self, app: &mut App) -> usize {
        self.inner.interactive.release(app) + app.release_scope(self.inner.scope)
    }

    /// 滑块位置 = 轨道内边距 + 进度 × 可移动距离（`LayoutOverride`，相对轨道 border box）。
    fn sync_knob(&self, app: &mut App) -> Result<bool, EngineError> {
        let (Some(window), Some(knob)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().knob,
        ) else {
            return Ok(false);
        };
        let size = self.size;
        let track = track_metrics(size);
        let mix = self.mix(app);
        let knob_size = track.height - KNOB_INSET * 2.0;
        let offset = KNOB_INSET + mix * (track.width - knob_size - KNOB_INSET * 2.0);
        let rect = Rect::new(offset, KNOB_INSET, knob_size, knob_size);
        let current = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree()
            .get(knob)
            .ok()
            .and_then(|target| target.layout_override())
            .map(|layout| layout.rect);
        if current == Some(rect) {
            return Ok(false);
        }
        app.window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut()
            .set_layout_override(knob, Some(LayoutOverride::new(rect)))?;
        Ok(true)
    }
}

impl UnifiedSizable for Toggle {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.size = size;
        self
    }
}

/// 滑块相对轨道的内边距。
const KNOB_INSET: f32 = 2.0;

/// 几何控制节点及其可选的滑块 / 勾（内点）。
struct Control {
    node: NodeId,
    knob: Option<NodeId>,
    /// 勾（Checkbox）或内点（Radio）：`opacity` 读 `mix` 瞬态，必须登记进动画目标。
    mark: Option<NodeId>,
}

/// Switch 轨道的尺寸。
struct TrackMetrics {
    width: f32,
    height: f32,
    radius: f32,
}

fn track_metrics(size: UnifiedSize) -> TrackMetrics {
    let height = size.icon_size() * 1.25;
    TrackMetrics {
        width: height * 1.9,
        height,
        radius: height / 2.0,
    }
}

/// 建几何控制节点（方框 / 圆框 / 轨道），含勾 / 点 / 滑块。
fn build_control(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    kind: ToggleKind,
    size: UnifiedSize,
    theme: ThemeHandle,
    mix: Transient<f32>,
    disabled: Signal<bool>,
) -> Result<Control, EngineError> {
    let colors_theme = theme.clone();
    let colors_mix = mix;
    match kind {
        ToggleKind::Checkbox | ToggleKind::Radio => {
            let side = size.icon_size() * 1.5;
            let radius = if kind == ToggleKind::Radio {
                side / 2.0
            } else {
                3.0
            };
            let control = NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .size(side, side)
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .build(),
                )
                .paint(PaintBuilder::new().rounded(radius).build())
                .bind_style(move |cx| {
                    let (palette, enabled) = toggle_colors(cx, &colors_theme, &disabled);
                    let progress = *cx.read_transient(&colors_mix);
                    let resolver = ColorResolver;
                    let background =
                        resolver.lerp_color(Color::TRANSPARENT, palette.checked, progress);
                    let border = resolver.lerp_color(palette.unchecked, palette.checked, progress);
                    StylePatch {
                        background: Some(background),
                        border: Some(Border {
                            width: 1.0,
                            color: if enabled { border } else { palette.unchecked },
                        }),
                        ..StylePatch::default()
                    }
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    parent,
                )?;
            let theme_mark = theme.clone();
            let mark_mix = mix;
            let mark = match kind {
                ToggleKind::Checkbox => Some(mount_dynamic_icon(
                    app,
                    window,
                    control,
                    IconKind::Check,
                    side * 0.7,
                    move |cx| {
                        let progress = *cx.read_transient(&mark_mix);
                        StylePatch {
                            foreground: Some(theme_mark.role(cx, Role::FgOnBrand)),
                            opacity: Some(progress),
                            ..StylePatch::default()
                        }
                    },
                )?),
                ToggleKind::Radio => {
                    let dot = side * 0.42;
                    let theme_dot = theme;
                    Some(
                        NodeBuilder::boxed()
                            .style(StyleBuilder::new().size(dot, dot).build())
                            .paint(PaintBuilder::new().rounded(dot / 2.0).build())
                            .bind_style(move |cx| {
                                let progress = *cx.read_transient(&mark_mix);
                                StylePatch {
                                    background: Some(theme_dot.role(cx, Role::FgOnBrand)),
                                    opacity: Some(progress),
                                    ..StylePatch::default()
                                }
                            })
                            .build(
                                app.window_mut(window)
                                    .ok_or(EngineError::UnknownWindow { window })?
                                    .tree_mut(),
                                control,
                            )?,
                    )
                }
                ToggleKind::Switch => None,
            };
            Ok(Control {
                node: control,
                knob: None,
                mark,
            })
        }
        ToggleKind::Switch => {
            let metrics = track_metrics(size);
            let theme_track = theme.clone();
            let track_mix = mix;
            let track = NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .size(metrics.width, metrics.height)
                        .relative()
                        .build(),
                )
                .paint(PaintBuilder::new().rounded(metrics.radius).build())
                .bind_style(move |cx| {
                    let (palette, _) = toggle_colors(cx, &theme_track, &disabled);
                    let progress = *cx.read_transient(&track_mix);
                    let resolver = ColorResolver;
                    let background =
                        resolver.lerp_color(palette.unchecked, palette.checked, progress);
                    StylePatch {
                        background: Some(background),
                        border: Some(Border {
                            width: 1.0,
                            color: background,
                        }),
                        ..StylePatch::default()
                    }
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    parent,
                )?;
            let knob_size = metrics.height - KNOB_INSET * 2.0;
            let initial = Rect::new(KNOB_INSET, KNOB_INSET, knob_size, knob_size);
            let theme_knob = theme;
            let knob = NodeBuilder::boxed()
                .layout_override(initial)
                .paint(PaintBuilder::new().rounded(knob_size / 2.0).build())
                .bind_style(move |cx| StylePatch {
                    background: Some(theme_knob.role(cx, Role::FgOnBrand)),
                    ..StylePatch::default()
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    track,
                )?;
            Ok(Control {
                node: track,
                knob: Some(knob),
                mark: None,
            })
        }
    }
}

/// 当前勾选 / 禁用状态下的主题开关配方。
fn toggle_colors(
    cx: &mut ReadCtx<'_>,
    theme: &ThemeHandle,
    disabled: &Signal<bool>,
) -> (ToggleColors, bool) {
    let components = theme.components(cx);
    let enabled = !*cx.read(disabled);
    let palette = if enabled {
        components.toggle.enabled
    } else {
        components.toggle.disabled
    };
    (palette, enabled)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use ui_core::{
        a11y::A11yRole,
        event::{InputEvent, KeyCode, Modifiers, PointerButton},
        math::Point,
        node::id::NodeId,
        paint::color::{Color, ColorResolver},
        test_support::app::TestApp,
    };

    use super::{Toggle, ToggleKind};
    use crate::theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role};

    struct Harness {
        app: TestApp,
        toggle: Toggle,
        node: NodeId,
        reported: Rc<Cell<Option<bool>>>,
    }

    fn harness_with(
        kind: ToggleKind,
        checked: bool,
        configure: impl FnOnce(Toggle) -> Toggle,
    ) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let reported = Rc::new(Cell::new(None));
        let sink = reported.clone();
        let toggle = Toggle::new(app.app_mut(), theme, "toggle")
            .checked(checked)
            .on_change(move |checked, _| {
                sink.set(Some(checked));
            });
        let toggle = match kind {
            ToggleKind::Checkbox => toggle.checkbox(),
            ToggleKind::Radio => toggle.radio(),
            ToggleKind::Switch => toggle.switch(),
        };
        let toggle = configure(toggle);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = toggle.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let _ = toggle.sync(app.app_mut());
        let _ = app.drain().expect("frame");
        Harness {
            app,
            toggle,
            node,
            reported,
        }
    }

    fn harness(kind: ToggleKind, checked: bool) -> Harness {
        harness_with(kind, checked, std::convert::identity)
    }

    fn settle(harness: &mut Harness) {
        for _ in 0..4 {
            harness.app.advance(std::time::Duration::from_millis(60));
            let _ = harness.app.drain().expect("frame");
            let _ = harness.toggle.sync(harness.app.app_mut());
        }
        harness.app.advance(std::time::Duration::from_millis(200));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.toggle.sync(harness.app.app_mut());
        let _ = harness.app.drain().expect("frame");
    }

    fn center(app: &TestApp, node: NodeId) -> Point {
        let bounds = app.probe().bounds(node).expect("bounds");
        Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn click(harness: &mut Harness) {
        let point = center(&harness.app, harness.node);
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
        let _ = harness.toggle.sync(harness.app.app_mut());
    }

    fn control_patch(harness: &Harness) -> ui_core::paint::style::StylePatch {
        let window = harness.app.window_id();
        harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(harness.toggle.control_node().expect("control"))
            .expect("control")
            .style_patch()
            .clone()
    }

    fn resolved(color: Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    fn mark_node(harness: &Harness) -> NodeId {
        harness
            .toggle
            .inner
            .geometry
            .borrow()
            .mark
            .expect("勾 / 内点节点")
    }

    fn mark_patch(harness: &Harness) -> ui_core::paint::style::StylePatch {
        let window = harness.app.window_id();
        harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(mark_node(harness))
            .expect("mark")
            .style_patch()
            .clone()
    }

    fn light_theme() -> Theme {
        Theme::of(Appearance::Light)
    }

    /// 点击切换勾选态：信号翻转、回调收到新值、勾选动画推进。
    #[test]
    fn clicking_flips_checked_reports_it_and_animates_the_mark() {
        let mut harness = harness(ToggleKind::Checkbox, false);
        assert!(!harness.toggle.is_checked(harness.app.app()));
        click(&mut harness);
        assert!(harness.toggle.is_checked(harness.app.app()));
        assert_eq!(harness.reported.get(), Some(true), "on_change 收到新值");
        settle(&mut harness);
        assert!(
            (harness.toggle.mix(harness.app.app()) - 1.0).abs() < 1.0e-4,
            "勾选进度到 1"
        );
        let components = light_theme().components().toggle.enabled;
        assert_eq!(
            control_patch(&harness).background.map(resolved),
            Some(resolved(components.checked)),
            "选中后的方框填充是主题 checked 色"
        );
    }

    /// 键盘 Space：焦点节点的 Space 激活与点击同一个 Action。
    #[test]
    fn space_on_the_focused_toggle_flips_it() {
        let mut harness = harness(ToggleKind::Switch, false);
        let window = harness.app.window_id();
        assert!(harness.app.app_mut().set_focus(window, Some(harness.node)));
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::Space, Modifiers::empty()));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::Space, Modifiers::empty()));
        assert!(harness.toggle.is_checked(harness.app.app()));
        assert_eq!(harness.reported.get(), Some(true));
    }

    /// 未选中：方框背景透明；选中：品牌色填充（插值两端）。
    #[test]
    fn the_box_fills_between_the_unchecked_and_checked_roles() {
        let unchecked_case = harness(ToggleKind::Checkbox, false);
        let components = light_theme().components().toggle.enabled;
        assert_eq!(
            control_patch(&unchecked_case).background.map(resolved),
            Some(resolved(Color::TRANSPARENT)),
            "未选中方框无填充"
        );
        assert_eq!(
            control_patch(&unchecked_case)
                .border
                .map(|border| resolved(border.color)),
            Some(resolved(components.unchecked)),
            "未选中方框边缘是主题 unchecked 色"
        );
        let mut checked_case = harness(ToggleKind::Checkbox, true);
        settle(&mut checked_case);
        assert_eq!(
            control_patch(&checked_case).background.map(resolved),
            Some(resolved(components.checked))
        );
    }

    /// Switch：滑块位置随勾选进度从轨道左侧移到右侧。
    #[test]
    fn the_switch_knob_moves_from_left_to_right() {
        let mut harness = harness(ToggleKind::Switch, false);
        let knob = harness.toggle.knob_node().expect("knob");
        let track = harness.toggle.control_node().expect("track");
        let window = harness.app.window_id();
        let left = |harness: &Harness| {
            harness
                .app
                .app()
                .window(window)
                .expect("window")
                .tree()
                .get(knob)
                .expect("knob")
                .layout_override()
                .expect("override")
                .rect
        };
        let before = left(&harness);
        assert_eq!(before.origin.x, super::KNOB_INSET, "未选中滑块贴左");
        click(&mut harness);
        settle(&mut harness);
        let after = left(&harness);
        assert!(after.origin.x > before.origin.x, "选中后滑块向右移动");
        let track_bounds = harness.app.probe().bounds(track).expect("track");
        assert!(
            after.origin.x + after.size.width <= track_bounds.size.width + 0.5,
            "滑块不得移出轨道"
        );
    }

    /// 勾 / 内点是动画目标：点击后 `opacity` 从 0 走到 1（变异：不登记 `watch` → 恒 0，本测试红）。
    #[test]
    fn clicking_fades_the_mark_in() {
        for kind in [ToggleKind::Checkbox, ToggleKind::Radio] {
            let mut harness = harness(kind, false);
            assert_eq!(
                mark_patch(&harness).opacity,
                Some(0.0),
                "{kind:?} 未选中时勾 / 内点透明"
            );
            click(&mut harness);
            settle(&mut harness);
            assert_eq!(
                mark_patch(&harness).opacity,
                Some(1.0),
                "{kind:?} 选中后勾 / 内点必须可见"
            );
        }
    }

    /// 根节点保留旧 `unified_sized` 的行高与水平内边距（`chrome(false)` 也要写）。
    #[test]
    fn the_root_keeps_the_old_row_height_and_left_padding() {
        for size in [
            crate::kit::size::UnifiedSize::Small,
            crate::kit::size::UnifiedSize::Medium,
            crate::kit::size::UnifiedSize::Large,
        ] {
            let harness = harness_with(ToggleKind::Checkbox, false, |toggle| {
                crate::kit::size::UnifiedSizable::with_unified_size(toggle, size)
            });
            let window = harness.app.window_id();
            let bounds = harness
                .app
                .probe()
                .bounds(harness.node)
                .expect("root bounds");
            assert_eq!(bounds.size.height, size.h(), "{size:?} 的行高");
            let style = harness
                .app
                .app()
                .window(window)
                .expect("window")
                .tree()
                .get(harness.node)
                .expect("root")
                .style();
            assert_eq!(
                style.inner.padding.left,
                taffy::style::LengthPercentage::length(size.px()),
                "{size:?} 的左内边距"
            );
        }
    }

    /// `checked` 双向：点击后的状态进 a11y 快照（变异：不读 `checked` 信号 → 快照停在 false）。
    #[test]
    fn clicking_updates_the_checked_state_in_the_a11y_snapshot() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let toggle = Toggle::new(app.app_mut(), theme, "checked-a11y").label("Enabled");
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = toggle.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let _ = toggle.sync(app.app_mut());
        assert_eq!(
            app.a11y()
                .expect("a11y")
                .node_for(node)
                .expect("a11y node")
                .checked,
            Some(false)
        );
        let point = center(&app, node);
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
        let _ = app.drain().expect("frame");
        assert_eq!(
            app.a11y()
                .expect("a11y")
                .node_for(node)
                .expect("a11y node")
                .checked,
            Some(true),
            "点击后 a11y 的 checked 必须更新为 true"
        );
    }

    /// a11y：三种变体的角色与 checked 状态都进快照。
    #[test]
    fn every_toggle_kind_declares_its_role_and_checked_state() {
        for (kind, role) in [
            (ToggleKind::Checkbox, A11yRole::CheckBox),
            (ToggleKind::Radio, A11yRole::RadioButton),
            (ToggleKind::Switch, A11yRole::Switch),
        ] {
            let mut app = TestApp::new();
            app.enable_a11y_capture();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let toggle = Toggle::new(app.app_mut(), theme, "a11y")
                .checked(false)
                .label("Enabled");
            let toggle = match kind {
                ToggleKind::Checkbox => toggle.checkbox(),
                ToggleKind::Radio => toggle.radio(),
                ToggleKind::Switch => toggle.switch(),
            };
            let window = app.window_id();
            let root = app.app().window(window).expect("window").tree().root();
            let node = toggle.build(app.app_mut(), window, root).expect("build");
            let _ = app.drain().expect("frame");
            let snapshot = app.a11y().expect("a11y");
            let record = snapshot.node_for(node).expect("a11y node");
            assert_eq!(record.role, role, "{kind:?} 的角色");
            assert_eq!(record.checked, Some(false));
            assert_eq!(record.label_text(), Some("Enabled"));
        }
    }

    /// 禁用：不响应点击，a11y 宣告 disabled。
    #[test]
    fn a_disabled_toggle_ignores_clicks_and_reports_disabled() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let reported = Rc::new(Cell::new(None));
        let sink = reported.clone();
        let toggle = Toggle::new(app.app_mut(), theme, "disabled")
            .disabled(true)
            .label("Enabled")
            .on_change(move |checked, _| {
                sink.set(Some(checked));
            });
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = toggle.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let point = center(&app, node);
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
        assert_eq!(reported.get(), None, "禁用开关不触发 on_change");
        assert!(!toggle.is_checked(app.app()));
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert!(record.disabled);
    }

    /// 标签：文字节点带字号；标签颜色走交互前景（禁用时 `FgDisabled`）。
    #[test]
    fn the_label_is_a_text_node_themed_by_the_interactive_foreground() {
        let harness = harness_with(ToggleKind::Checkbox, false, |toggle| {
            toggle.label("Remember")
        });
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        let children = tree.get(harness.node).expect("node").children();
        assert_eq!(children.len(), 2, "控制 + 标签");
        let label = *children.last().expect("label");
        match tree.get(label).expect("label").kind() {
            ui_core::node::tree::NodeKind::Text(content) => {
                assert_eq!(content.text, "Remember");
                assert_eq!(
                    content.style.size,
                    crate::kit::size::UnifiedSize::Medium.text_size()
                );
            }
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(
            tree.get(label)
                .expect("label")
                .style_patch()
                .foreground
                .map(resolved),
            Some(resolved(light_theme().role(Role::FgDefault)))
        );
    }

    /// I3：动画结束后的稳态不出帧。
    #[test]
    fn a_settled_toggle_produces_no_further_frames() {
        let mut harness = harness(ToggleKind::Switch, false);
        click(&mut harness);
        settle(&mut harness);
        assert!(
            !harness.toggle.sync(harness.app.app_mut()).expect("sync"),
            "稳态 sync 必须返回 false"
        );
        let record = harness.app.drain().expect("idle").clone();
        assert!(!record.rendered, "稳态不得出帧（I3）");
    }
}
