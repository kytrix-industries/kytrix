use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::SmolStr;
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    event::Event,
    interaction::ActionCtx,
    layout::LayoutOverride,
    math::{LogicalPx, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        image::ImageFit,
        signal::{Signal, SignalScope, Transient},
    },
    paint::{
        color::{Color, ColorResolver},
        primitive::Border,
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        interactive::Interactive,
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{components::ToggleColors, handle::ThemeHandle, role::Role},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToggleKind {
    #[default]
    Checkbox,
    Radio,
    Switch,
}

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
    mix_target: Cell<f32>,
    geometry: Rc<RefCell<Geometry>>,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    node: Option<NodeId>,
    control: Option<NodeId>,
    knob: Option<NodeId>,
    ring: Option<NodeId>,
    mark: Option<NodeId>,
}

impl Toggle {
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

    #[must_use]
    pub fn checkbox(mut self) -> Self {
        self.kind = ToggleKind::Checkbox;
        self
    }

    #[must_use]
    pub fn radio(mut self) -> Self {
        self.kind = ToggleKind::Radio;
        self
    }

    #[must_use]
    pub fn switch(mut self) -> Self {
        self.kind = ToggleKind::Switch;
        self
    }

    #[must_use]
    pub fn checked(mut self, checked: bool) -> Self {
        self.initial_checked = checked;
        self
    }

    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    #[must_use]
    pub fn on_change(mut self, callback: impl Fn(bool, &mut App) + 'static) -> Self {
        self.callback = Some(Rc::new(callback));
        self
    }

    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().node
    }

    #[must_use]
    pub fn control_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().control
    }

    #[must_use]
    pub fn knob_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().knob
    }

    #[must_use]
    pub fn is_checked(&self, app: &App) -> bool {
        app.read_with(&self.inner.checked, |checked| *checked)
    }

    #[must_use]
    pub fn mix(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.mix)
    }

    pub fn set_checked(&self, app: &mut App, checked: bool) -> bool {
        app.write(&self.inner.checked, checked)
    }

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
            if let Some(ring) = control.ring {
                watcher.watch(ring);
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
                style.flex_shrink(1.0);
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
            target.ring = control.ring;
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
        changed |= match self.kind {
            ToggleKind::Switch => self.sync_knob(app)?,
            ToggleKind::Checkbox | ToggleKind::Radio => false,
        };
        Ok(changed)
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.inner.interactive.dispatch(app, event)
    }

    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        self.inner.interactive.set_disabled(app, disabled)
    }

    pub fn release(&self, app: &mut App) -> usize {
        self.inner.interactive.release(app) + app.release_scope(self.inner.scope)
    }

    fn sync_knob(&self, app: &mut App) -> Result<bool, EngineError> {
        let (Some(window), Some(track), Some(knob)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().control,
            self.inner.geometry.borrow().knob,
        ) else {
            return Ok(false);
        };
        let Some(bounds) = layout_bounds(app, window, track) else {
            return Ok(false);
        };
        let inset = track_metrics(self.size).inset;
        let knob_size = (bounds.size.height - inset * 2.0)
            .min(bounds.size.width - inset * 2.0)
            .max(px(1.0));
        let travel = (bounds.size.width - knob_size - inset * 2.0).max(px(0.0));
        let offset = (inset + travel * self.mix(app))
            .value()
            .clamp(0.0, (bounds.size.width - knob_size).max(px(0.0)).value());
        let top = inset.min((bounds.size.height - knob_size).max(px(0.0)));
        set_override(
            app,
            window,
            knob,
            LogicalRect::new(px(offset), top, knob_size, knob_size),
        )
    }
}

fn layout_bounds(app: &App, window: WindowId, node: NodeId) -> Option<LogicalRect> {
    app.window(window)?
        .tree()
        .get(node)
        .ok()
        .map(|target| target.layout().bounds)
        .filter(|bounds| !bounds.is_empty())
}

fn set_override(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    rect: LogicalRect,
) -> Result<bool, EngineError> {
    let current = app
        .window(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree()
        .get(node)
        .ok()
        .and_then(|target| target.layout_override())
        .map(|layout| layout.rect);
    if current == Some(rect) {
        return Ok(false);
    }
    app.window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut()
        .set_layout_override(node, Some(LayoutOverride::new(rect)))?;
    Ok(true)
}

impl UnifiedSizable for Toggle {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.size = size;
        self
    }
}

const CONTROL_SIDE_EXTRA: LogicalPx = LogicalPx::new(4.0);

const MIN_CONTROL_SIDE: LogicalPx = LogicalPx::new(16.0);

const MARK_INSET: LogicalPx = LogicalPx::new(4.0);

const SWITCH_INSET: LogicalPx = LogicalPx::new(4.0);

const SWITCH_HEIGHT_EXTRA: LogicalPx = LogicalPx::new(8.0);

const CHECK_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><path d="M1.5 8.25 L5.5 12.25 L14.5 2.5" fill="none" stroke="#FFFFFF" stroke-width="3.5" stroke-linecap="round" stroke-linejoin="round"/></svg>"##;

const CHECK_INTRINSIC_SIZE: LogicalSize = LogicalSize::new(px(16.0), px(16.0));

struct Control {
    node: NodeId,
    knob: Option<NodeId>,
    ring: Option<NodeId>,
    mark: Option<NodeId>,
}

struct TrackMetrics {
    width: LogicalPx,
    height: LogicalPx,
    radius: LogicalPx,
    inset: LogicalPx,
}

fn control_side(size: UnifiedSize) -> LogicalPx {
    (size.icon_size() + CONTROL_SIDE_EXTRA).max(MIN_CONTROL_SIDE)
}

fn track_metrics(size: UnifiedSize) -> TrackMetrics {
    let height = size.icon_size() + SWITCH_HEIGHT_EXTRA;
    TrackMetrics {
        width: height * 2.0 - px(4.0),
        height,
        radius: height / 2.0,
        inset: SWITCH_INSET,
    }
}

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
    match kind {
        ToggleKind::Checkbox | ToggleKind::Radio => {
            let is_radio = kind == ToggleKind::Radio;
            let side = control_side(size);
            let mark_size = side - MARK_INSET * 2.0;
            let control_theme = theme.clone();
            let control_mix = mix;
            let control = NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .size(side, side)
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .build(),
                )
                .paint(PaintBuilder::new().rounded(px(3.0)).build())
                .bind_style(move |cx| {
                    if is_radio {
                        return StylePatch::default();
                    }
                    let (palette, enabled) = toggle_colors(cx, &control_theme, &disabled);
                    let progress = *cx.read_transient(&control_mix);
                    let resolver = ColorResolver;
                    let background =
                        resolver.lerp_color(Color::TRANSPARENT, palette.checked, progress);
                    let border = resolver.lerp_color(palette.unchecked, palette.checked, progress);
                    StylePatch {
                        background: Some(background),
                        border: Some(Border {
                            width: px(1.0),
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
            let ring = if is_radio {
                let ring_theme = theme.clone();
                let ring_mix = mix;
                Some(
                    NodeBuilder::boxed()
                        .style(StyleBuilder::new().size(side, side).build())
                        .layout_override(LogicalRect::new(
                            LogicalPx::ZERO,
                            LogicalPx::ZERO,
                            side,
                            side,
                        ))
                        .paint(PaintBuilder::new().rounded(side / 2.0).build())
                        .bind_style(move |cx| {
                            let (palette, enabled) = toggle_colors(cx, &ring_theme, &disabled);
                            let progress = *cx.read_transient(&ring_mix);
                            let resolver = ColorResolver;
                            let background =
                                resolver.lerp_color(Color::TRANSPARENT, palette.checked, progress);
                            let border =
                                resolver.lerp_color(palette.unchecked, palette.checked, progress);
                            StylePatch {
                                background: Some(background),
                                border: Some(Border {
                                    width: px(1.0),
                                    color: if enabled { border } else { palette.unchecked },
                                }),
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
            } else {
                None
            };
            let mark_mix = mix;
            let mark = match kind {
                ToggleKind::Checkbox => {
                    let initial = theme.theme_for(theme.current(app)).role(Role::FgOnBrand);
                    let resource = app.insert_svg_for_window(
                        window,
                        CHECK_SVG.as_bytes(),
                        CHECK_INTRINSIC_SIZE,
                    )?;
                    let mark_theme = theme;
                    Some(
                        NodeBuilder::image(resource.id(), ImageFit::Fill)
                            .image_modulation(initial)
                            .style(StyleBuilder::new().size(mark_size, mark_size).build())
                            .layout_override(LogicalRect::new(
                                MARK_INSET, MARK_INSET, mark_size, mark_size,
                            ))
                            .a11y(A11yOverride {
                                hidden: true,
                                ..A11yOverride::default()
                            })
                            .bind_style(move |cx| StylePatch {
                                foreground: Some(mark_theme.role(cx, Role::FgOnBrand)),
                                opacity: Some(*cx.read_transient(&mark_mix)),
                                ..StylePatch::default()
                            })
                            .build(
                                app.window_mut(window)
                                    .ok_or(EngineError::UnknownWindow { window })?
                                    .tree_mut(),
                                control,
                            )?,
                    )
                }
                ToggleKind::Radio => {
                    let theme_dot = theme;
                    Some(
                        NodeBuilder::boxed()
                            .style(StyleBuilder::new().size(mark_size, mark_size).build())
                            .layout_override(LogicalRect::new(
                                MARK_INSET, MARK_INSET, mark_size, mark_size,
                            ))
                            .paint(PaintBuilder::new().rounded(mark_size / 2.0).build())
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
                ring,
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
                            width: px(1.0),
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
            let knob_size = metrics.height - metrics.inset * 2.0;
            let initial = LogicalRect::new(metrics.inset, metrics.inset, knob_size, knob_size);
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
                ring: None,
                mark: None,
            })
        }
    }
}

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
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{
            color::{Color, ColorResolver},
            style_builder::StyleBuilder,
        },
        test_support::{app::TestApp, diagnostics::frame_dump},
    };

    use super::{Toggle, ToggleKind};
    use crate::{
        kit::test_support::test_app_with_scale,
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

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
        harness_at_scale(1.0, kind, checked, configure)
    }

    fn harness_at_scale(
        scale: f32,
        kind: ToggleKind,
        checked: bool,
        configure: impl FnOnce(Toggle) -> Toggle,
    ) -> Harness {
        harness_in(test_app_with_scale(scale), None, kind, checked, configure)
    }

    fn harness_in(
        mut app: TestApp,
        parent_width: Option<f32>,
        kind: ToggleKind,
        checked: bool,
        configure: impl FnOnce(Toggle) -> Toggle,
    ) -> Harness {
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
        let parent = parent_width.map_or(root, |width| {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(width))
                        .height(px(30.0))
                        .flex_col()
                        .build(),
                )
                .build(
                    app.app_mut().window_mut(window).expect("window").tree_mut(),
                    root,
                )
                .expect("窄容器")
        });
        let node = toggle.build(app.app_mut(), window, parent).expect("build");
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

    fn center(app: &TestApp, node: NodeId) -> LogicalPoint {
        let bounds = app.probe().bounds(node).expect("bounds");
        LogicalPoint::new(
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

    fn ring_node(harness: &Harness) -> NodeId {
        harness
            .toggle
            .inner
            .geometry
            .borrow()
            .ring
            .expect("单选外圈节点")
    }

    fn override_rect(harness: &Harness, node: NodeId) -> LogicalRect {
        let window = harness.app.window_id();
        harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("开关几何节点")
            .layout_override()
            .expect("布局覆盖")
            .rect
    }

    fn override_geometry(harness: &Harness) -> Vec<LogicalRect> {
        let geometry = harness.toggle.inner.geometry.borrow();
        [geometry.knob, geometry.ring, geometry.mark]
            .into_iter()
            .flatten()
            .map(|node| override_rect(harness, node))
            .collect()
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

    fn mark_alpha(harness: &Harness) -> Option<f32> {
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        match tree.get(mark_node(harness)).expect("mark").kind() {
            ui_core::node::tree::NodeKind::Canvas(canvas) => {
                canvas
                    .primitives()
                    .iter()
                    .find_map(|primitive| match primitive {
                        ui_core::paint::primitive::Primitive::Quad(quad) => match &quad.paint {
                            ui_core::paint::primitive::QuadPaint::Solid { fill, .. } => {
                                Some(ColorResolver.resolve(*fill).components[3])
                            }
                            _ => None,
                        },
                        _ => None,
                    })
            }
            _ => mark_patch(harness).opacity,
        }
    }

    fn light_theme() -> Theme {
        Theme::of(Appearance::Light)
    }

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

    #[test]
    fn the_switch_knob_moves_from_left_to_right() {
        let mut harness = harness(ToggleKind::Switch, false);
        let knob = harness.toggle.knob_node().expect("knob");
        let track = harness.toggle.control_node().expect("track");
        let before = override_rect(&harness, knob);
        assert_eq!(
            before.origin.x,
            super::track_metrics(crate::kit::size::UnifiedSize::Medium).inset,
            "未选中滑块贴左"
        );
        click(&mut harness);
        settle(&mut harness);
        let after = override_rect(&harness, knob);
        assert!(after.origin.x > before.origin.x, "选中后滑块向右移动");
        let track_bounds = harness.app.probe().bounds(track).expect("track");
        assert!(
            after.origin.x + after.size.width <= track_bounds.size.width + 0.5,
            "滑块不得移出轨道"
        );
    }

    #[test]
    fn clicking_fades_the_mark_in() {
        for kind in [ToggleKind::Checkbox, ToggleKind::Radio] {
            let mut harness = harness(kind, false);
            assert_eq!(
                mark_alpha(&harness),
                Some(0.0),
                "{kind:?} 未选中时勾 / 内点透明"
            );
            click(&mut harness);
            settle(&mut harness);
            assert_eq!(
                mark_alpha(&harness),
                Some(1.0),
                "{kind:?} 选中后勾 / 内点必须可见"
            );
        }
    }

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
                style.inner().padding.left,
                taffy::style::LengthPercentage::length(size.px().value()),
                "{size:?} 的左内边距"
            );
        }
    }

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

    #[test]
    fn toggle_logical_geometry_is_stable_at_every_dpi() {
        for kind in [ToggleKind::Switch, ToggleKind::Checkbox, ToggleKind::Radio] {
            let origin = harness_at_scale(1.0, kind, true, std::convert::identity);
            assert_eq!(
                frame_dump(&origin.app).scale,
                1.0,
                "{kind:?}: 基准 DPI 注入必须生效"
            );
            let baseline = override_geometry(&origin);
            assert!(
                !baseline.is_empty(),
                "{kind:?}: Toggle 必须有 LayoutOverride 几何"
            );
            for scale in [1.25f32, 1.5, 2.0, 3.0] {
                let scaled = harness_at_scale(scale, kind, true, std::convert::identity);
                assert_eq!(
                    frame_dump(&scaled.app).scale,
                    scale,
                    "{kind:?} @ {scale}: DPI 注入必须生效"
                );
                assert_eq!(
                    override_geometry(&scaled),
                    baseline,
                    "{kind:?} @ {scale}: LayoutOverride 逻辑几何必须与 DPI 无关"
                );
            }
        }
    }

    #[test]
    fn the_medium_toggle_geometry_is_frozen_in_logical_pixels() {
        let metrics = super::track_metrics(crate::kit::size::UnifiedSize::Medium);
        assert_eq!(metrics.width, px(36.0));
        assert_eq!(metrics.height, px(20.0));
        assert_eq!(metrics.radius, px(10.0));
        assert_eq!(metrics.inset, px(4.0));
        let travel = metrics.width - (metrics.height - metrics.inset * 2.0) - metrics.inset * 2.0;
        assert_eq!(travel, px(16.0));

        let unchecked = harness(ToggleKind::Switch, false);
        let track = unchecked
            .app
            .probe()
            .bounds(unchecked.toggle.control_node().expect("track"))
            .expect("track bounds");
        assert_eq!(
            track.size,
            LogicalSize::new(px(36.0), px(20.0)),
            "switch 轨道逻辑尺寸"
        );
        let knob = unchecked.toggle.knob_node().expect("knob");
        assert_eq!(
            override_rect(&unchecked, knob),
            LogicalRect::new(px(4.0), px(4.0), px(12.0), px(12.0)),
            "未选中滑块几何"
        );

        let mut checked = harness(ToggleKind::Switch, true);
        checked.app.invalidate_all_lanes();
        let record = checked.app.drain().expect("frame").clone();
        let rendered = record.frame.as_ref().expect("全量重绘必须提交绘制快照");
        let track_quad = rendered
            .quads
            .iter()
            .find(|quad| quad.node == checked.toggle.control_node().expect("track"))
            .expect("轨道四边图元");
        assert_eq!(
            track_quad.corner_radius,
            [px(10.0); 4],
            "switch 轨道圆角半径"
        );
        assert_eq!(
            override_rect(&checked, checked.toggle.knob_node().expect("knob")),
            LogicalRect::new(px(20.0), px(4.0), px(12.0), px(12.0)),
            "选中滑块几何：inset 4 + travel 16"
        );

        let checkbox = harness(ToggleKind::Checkbox, true);
        let control = checkbox
            .app
            .probe()
            .bounds(checkbox.toggle.control_node().expect("control"))
            .expect("control bounds");
        assert_eq!(
            control.size,
            LogicalSize::new(px(16.0), px(16.0)),
            "checkbox 方框逻辑尺寸"
        );
        let mark = override_rect(&checkbox, mark_node(&checkbox));
        assert_eq!(
            mark,
            LogicalRect::new(px(4.0), px(4.0), px(8.0), px(8.0)),
            "checkbox mark 几何"
        );

        let radio = harness(ToggleKind::Radio, true);
        assert_eq!(
            override_rect(&radio, ring_node(&radio)),
            LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
            "单选外圈几何"
        );
        let dot = override_rect(&radio, mark_node(&radio));
        assert_eq!(
            dot,
            LogicalRect::new(px(4.0), px(4.0), px(8.0), px(8.0)),
            "单选内点几何"
        );
    }

    #[test]
    fn a_narrow_row_clamps_the_knob_inside_the_track() {
        for width in [20.0f32, 30.0, 35.0] {
            let harness = harness_in(
                test_app_with_scale(1.0),
                Some(width),
                ToggleKind::Switch,
                true,
                std::convert::identity,
            );
            let track = harness
                .app
                .probe()
                .bounds(harness.toggle.control_node().expect("track"))
                .expect("track bounds");
            let knob = override_rect(&harness, harness.toggle.knob_node().expect("knob"));
            assert!(
                track.size.width.value() < 36.0,
                "宽度 {width}: track 必须确实被压窄（实际 {}）",
                track.size.width
            );
            assert!(knob.origin.x.value() >= 0.0, "宽度 {width}: 滑块左缘越界");
            assert!(
                knob.origin.x + knob.size.width <= track.size.width + 1e-4,
                "宽度 {width}: 滑块右缘 {} 超出轨道宽 {}",
                knob.origin.x + knob.size.width,
                track.size.width
            );
            assert!(
                knob.origin.y + knob.size.height <= track.size.height + 1e-4,
                "宽度 {width}: 滑块下缘 {} 超出轨道高 {}",
                knob.origin.y + knob.size.height,
                track.size.height
            );
        }
    }

    #[test]
    fn the_track_and_knob_physical_bounds_are_frozen_device_pixels() {
        let expected = [
            (1.0f32, [6.0f32, 5.0, 36.0, 20.0], [26.0, 9.0, 12.0, 12.0]),
            (1.25, [8.0, 6.0, 45.0, 25.0], [33.0, 11.0, 15.0, 15.0]),
            (1.5, [9.0, 8.0, 54.0, 30.0], [39.0, 14.0, 18.0, 18.0]),
            (1.75, [11.0, 9.0, 63.0, 35.0], [46.0, 16.0, 21.0, 21.0]),
            (2.0, [12.0, 10.0, 72.0, 40.0], [52.0, 18.0, 24.0, 24.0]),
            (3.0, [18.0, 15.0, 108.0, 60.0], [78.0, 27.0, 36.0, 36.0]),
        ];
        for (scale, track_px, knob_px) in expected {
            let [_, track_y, _, track_h] = track_px;
            let [_, knob_y, _, knob_h] = knob_px;
            assert_eq!(
                knob_y - track_y,
                track_h - (knob_y - track_y) - knob_h,
                "switch @ {scale}: 滑块上下边距必须相等"
            );
            let mut harness =
                harness_at_scale(scale, ToggleKind::Switch, true, std::convert::identity);
            harness.app.invalidate_all_lanes();
            let record = harness.app.drain().expect("frame").clone();
            let rendered = record.frame.as_ref().expect("全量重绘必须提交绘制快照");
            let dump = frame_dump(&harness.app);
            assert_eq!(dump.scale, scale, "DPI 注入必须生效");
            let checks = [
                (
                    "track",
                    harness.toggle.control_node().expect("track"),
                    LogicalRect::new(px(6.0), px(5.0), px(36.0), px(20.0)),
                    track_px,
                ),
                (
                    "knob",
                    harness.toggle.knob_node().expect("knob"),
                    LogicalRect::new(px(26.0), px(9.0), px(12.0), px(12.0)),
                    knob_px,
                ),
            ];
            for (label, node, logical, physical) in checks {
                let quad = rendered
                    .quads
                    .iter()
                    .find(|quad| quad.node == node)
                    .unwrap_or_else(|| panic!("{label} @ {scale}: 帧记录里必须有四边图元"));
                assert_eq!(quad.bounds, logical, "{label} @ {scale}: 逻辑四边");
                let instance = dump
                    .instances
                    .iter()
                    .find(|instance| instance.bounds == physical)
                    .unwrap_or_else(|| {
                        panic!(
                            "{label} @ {scale}: 编码后的物理边界必须是 {physical:?}，实际 {:?}",
                            dump.instances
                                .iter()
                                .map(|instance| instance.bounds)
                                .collect::<Vec<_>>()
                        )
                    });
                assert!(
                    instance.bounds.iter().all(|value| value.fract() == 0.0),
                    "{label} @ {scale}: 物理边界必须是整数设备像素 {:?}",
                    instance.bounds
                );
            }
        }
    }

    #[test]
    fn the_checkbox_mark_is_a_vector_image() {
        let harness = harness(ToggleKind::Checkbox, true);
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        match tree.get(mark_node(&harness)).expect("mark").kind() {
            ui_core::node::tree::NodeKind::Image(content) => {
                assert_eq!(
                    content.fit(),
                    ui_core::node::image::ImageFit::Fill,
                    "勾必须按标记盒填充矢量资源"
                );
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn the_toggle_shapes_stay_square_and_centered_in_device_pixels() {
        let expected = [
            (1.0f32, [6.0f32, 7.0, 16.0, 16.0], [10.0, 11.0, 8.0, 8.0]),
            (1.25, [8.0, 9.0, 20.0, 20.0], [13.0, 14.0, 10.0, 10.0]),
            (1.5, [9.0, 11.0, 24.0, 24.0], [15.0, 17.0, 12.0, 12.0]),
            (1.75, [11.0, 12.0, 28.0, 28.0], [18.0, 19.0, 14.0, 14.0]),
            (2.0, [12.0, 14.0, 32.0, 32.0], [20.0, 22.0, 16.0, 16.0]),
            (3.0, [18.0, 21.0, 48.0, 48.0], [30.0, 33.0, 24.0, 24.0]),
        ];
        for kind in [ToggleKind::Checkbox, ToggleKind::Radio] {
            for (scale, control_px, mark_px) in expected {
                let mut harness = harness_at_scale(scale, kind, true, std::convert::identity);
                harness.app.invalidate_all_lanes();
                let _ = harness.app.drain().expect("frame");
                let dump = frame_dump(&harness.app);
                let control = dump
                    .instances
                    .iter()
                    .find(|instance| instance.bounds == control_px)
                    .unwrap_or_else(|| {
                        panic!(
                            "{kind:?} @ {scale}: 找不到物理边界 {control_px:?}，实际 {:?}",
                            dump.instances
                                .iter()
                                .map(|instance| instance.bounds)
                                .collect::<Vec<_>>()
                        )
                    });
                let mark = dump
                    .instances
                    .iter()
                    .find(|instance| instance.bounds == mark_px)
                    .unwrap_or_else(|| {
                        panic!(
                            "{kind:?} @ {scale}: 找不到标记物理边界 {mark_px:?}，实际 {:?}",
                            dump.instances
                                .iter()
                                .map(|instance| instance.bounds)
                                .collect::<Vec<_>>()
                        )
                    });
                let [cx, cy, cw, ch] = control.bounds;
                let [mx, my, mw, mh] = mark.bounds;
                assert_eq!(cw, ch, "{kind:?} @ {scale}: 外框必须是正圆/正方形");
                assert_eq!(mw, mh, "{kind:?} @ {scale}: 标记必须是正圆/正方形");
                assert_eq!(
                    mx - cx,
                    cw - (mx - cx) - mw,
                    "{kind:?} @ {scale}: 标记必须水平居中"
                );
                assert_eq!(
                    my - cy,
                    ch - (my - cy) - mh,
                    "{kind:?} @ {scale}: 标记必须垂直居中"
                );
            }
        }
    }

    #[test]
    fn a_settled_toggle_produces_no_further_frames() {
        for kind in [ToggleKind::Switch, ToggleKind::Checkbox, ToggleKind::Radio] {
            let mut harness = harness(kind, false);
            click(&mut harness);
            settle(&mut harness);
            assert!(
                !harness.toggle.sync(harness.app.app_mut()).expect("sync"),
                "{kind:?} 稳态 sync 必须返回 false"
            );
            let record = harness.app.drain().expect("idle").clone();
            assert!(!record.rendered, "{kind:?} 稳态不得出帧（I3）");
        }
    }
}
