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
    math::{Point, Rect},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
    },
    paint::{
        canvas::Canvas,
        color::{Color, ColorResolver},
        primitive::{Border, Quad, QuadPaint},
        style::StylePatch,
    },
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        interactive::Interactive,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::{PaintBuilder, StyleBuilder},
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
    mark_paint: Cell<Option<(Rect, Color)>>,
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
            target.ring = control.ring;
            target.mark = control.mark;
            target.mark_paint.set(control.mark_paint);
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
            ToggleKind::Checkbox => self.sync_mark(app)?,
            ToggleKind::Radio => {
                let mut radio_changed = self.sync_ring(app)?;
                radio_changed |= self.sync_mark(app)?;
                radio_changed
            }
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
        let Some(bounds) = node_bounds(app, window, track) else {
            return Ok(false);
        };
        let scale = window_scale(app, window);
        let track_width = (bounds.size.width * scale).round().max(1.0);
        let track_height = (bounds.size.height * scale).round().max(1.0);
        let inset = (KNOB_INSET * scale).round().max(1.0);
        let knob_size = (track_height - inset * 2.0).max(1.0);
        let travel = (track_width - knob_size - inset * 2.0).max(0.0);
        let offset = inset + self.mix(app) * travel;
        let rect = Rect::new(
            offset / scale,
            inset / scale,
            knob_size / scale,
            knob_size / scale,
        );
        set_override(app, window, knob, rect)
    }

    fn sync_ring(&self, app: &mut App) -> Result<bool, EngineError> {
        let (Some(window), Some(control), Some(ring)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().control,
            self.inner.geometry.borrow().ring,
        ) else {
            return Ok(false);
        };
        let Some(bounds) = node_bounds(app, window, control) else {
            return Ok(false);
        };
        let scale = window_scale(app, window);
        let width = (bounds.size.width * scale).round().max(1.0);
        let height = (bounds.size.height * scale).round().max(1.0);
        let diameter = width.min(height);
        let x = ((width - diameter) / 2.0).round() / scale;
        let y = ((height - diameter) / 2.0).round() / scale;
        let rect = Rect::new(x, y, diameter / scale, diameter / scale);
        set_override(app, window, ring, rect)
    }

    fn sync_mark(&self, app: &mut App) -> Result<bool, EngineError> {
        if self.kind == ToggleKind::Switch {
            return Ok(false);
        }
        let (Some(window), Some(control), Some(mark)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().control,
            self.inner.geometry.borrow().mark,
        ) else {
            return Ok(false);
        };
        let Some(control_bounds) = node_bounds(app, window, control) else {
            return Ok(false);
        };
        let scale = window_scale(app, window);
        let ring = self.inner.geometry.borrow().ring;
        let container = if self.kind == ToggleKind::Radio {
            ring.and_then(|ring| node_bounds(app, window, ring))
        } else {
            None
        };
        let (offset_x, offset_y, width, height) = match container {
            Some(bounds) => (
                bounds.origin.x - control_bounds.origin.x,
                bounds.origin.y - control_bounds.origin.y,
                (bounds.size.width * scale).round().max(1.0),
                (bounds.size.height * scale).round().max(1.0),
            ),
            None => (
                0.0,
                0.0,
                (control_bounds.size.width * scale).round().max(1.0),
                (control_bounds.size.height * scale).round().max(1.0),
            ),
        };
        let ratio = if self.kind == ToggleKind::Radio {
            RADIO_DOT_RATIO
        } else {
            CHECK_ICON_RATIO
        };
        let mut mark_size = (width * ratio).round().max(1.0);
        if ((mark_size as i64 - width as i64) & 1) != 0 {
            mark_size += 1.0;
        }
        let x = offset_x + ((width - mark_size) / 2.0).round() / scale;
        let y = offset_y + ((height - mark_size) / 2.0).round() / scale;
        let mark_size = mark_size / scale;
        let rect = Rect::new(x, y, mark_size, mark_size);
        let mut changed = set_override(app, window, mark, rect)?;
        if self.kind == ToggleKind::Checkbox {
            let appearance = self.inner.theme.current(app);
            let color = self
                .inner
                .theme
                .theme_for(appearance)
                .role(Role::FgOnBrand)
                .with_opacity(self.mix(app));
            let paint_bounds = Rect::new(
                control_bounds.origin.x + x,
                control_bounds.origin.y + y,
                mark_size,
                mark_size,
            );
            let paint = (paint_bounds, color);
            if self.inner.geometry.borrow().mark_paint.get() != Some(paint) {
                let canvas = check_canvas(paint_bounds, color);
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .set_canvas(mark, canvas)?;
                self.inner.geometry.borrow_mut().mark_paint.set(Some(paint));
                changed = true;
            }
        }
        Ok(changed)
    }
}

fn node_bounds(app: &App, window: WindowId, node: NodeId) -> Option<Rect> {
    app.window(window)?
        .tree()
        .get(node)
        .ok()
        .map(|target| target.layout().bounds)
        .filter(|bounds| !bounds.is_empty())
}

fn window_scale(app: &App, window: WindowId) -> f32 {
    app.window(window)
        .map_or(1.0, |target| target.scale().factor())
}

fn device_length(value: f32, scale: f32) -> f32 {
    if !scale.is_finite() || scale <= 0.0 {
        return value;
    }
    (value * scale).round().max(1.0) / scale
}

fn device_length_with_parity(value: f32, scale: f32, reference: f32) -> f32 {
    if !scale.is_finite() || scale <= 0.0 {
        return value;
    }
    let mut length = (value * scale).round().max(1.0);
    if ((length as i64 - reference as i64) & 1) != 0 {
        length += 1.0;
    }
    length / scale
}

fn check_canvas(bounds: Rect, color: Color) -> Canvas {
    let mut canvas = Canvas::new(bounds);
    let size = bounds.size.width.min(bounds.size.height);
    let radius = size * CHECK_STROKE_RATIO;
    let points = [
        Point::new(bounds.origin.x + size * 0.20, bounds.origin.y + size * 0.55),
        Point::new(bounds.origin.x + size * 0.43, bounds.origin.y + size * 0.78),
        Point::new(bounds.origin.x + size * 0.82, bounds.origin.y + size * 0.22),
    ];
    for pair in points.windows(2) {
        let from = pair[0];
        let to = pair[1];
        let length = (to.x - from.x).hypot(to.y - from.y);
        let steps = (length / (radius * 0.6)).ceil().max(1.0) as usize;
        for step in 0..=steps {
            let t = step as f32 / steps as f32;
            let center = Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
            canvas.push_quad(Quad {
                bounds: Rect::new(
                    center.x - radius,
                    center.y - radius,
                    radius * 2.0,
                    radius * 2.0,
                ),
                corner_radius: [radius; 4],
                paint: QuadPaint::Solid {
                    fill: color,
                    border: None,
                },
            });
        }
    }
    canvas
}

fn set_override(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    rect: Rect,
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

const KNOB_INSET: f32 = 2.0;

const RADIO_DOT_RATIO: f32 = 0.42;

const CHECK_ICON_RATIO: f32 = 0.9;

const CHECK_STROKE_RATIO: f32 = 0.075;

struct Control {
    node: NodeId,
    knob: Option<NodeId>,
    ring: Option<NodeId>,
    mark: Option<NodeId>,
    mark_paint: Option<(Rect, Color)>,
}

struct TrackMetrics {
    width: f32,
    height: f32,
    radius: f32,
    inset: f32,
}

fn track_metrics(size: UnifiedSize, scale: f32) -> TrackMetrics {
    let height = device_length(size.icon_size() * 1.25, scale);
    TrackMetrics {
        width: device_length(height * 1.9, scale),
        height,
        radius: height / 2.0,
        inset: device_length(KNOB_INSET, scale),
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
            let scale = window_scale(app, window);
            let side = device_length(size.icon_size() * 1.5, scale);
            let side_px = (side * scale).round().max(1.0);
            let is_radio = kind == ToggleKind::Radio;
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
                .paint(PaintBuilder::new().rounded(3.0).build())
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
            let ring = if is_radio {
                let ring_theme = theme.clone();
                let ring_mix = mix;
                Some(
                    NodeBuilder::boxed()
                        .style(StyleBuilder::new().size(side, side).build())
                        .layout_override(Rect::new(0.0, 0.0, side, side))
                        .paint(PaintBuilder::new().rounded(side).build())
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
                            control,
                        )?,
                )
            } else {
                None
            };
            let mark_mix = mix;
            let (mark, mark_paint) = match kind {
                ToggleKind::Checkbox => {
                    let mark_size =
                        device_length_with_parity(side * CHECK_ICON_RATIO, scale, side_px);
                    let inset = (side - mark_size) / 2.0;
                    let canvas = Canvas::new(Rect::default());
                    let mark = NodeBuilder::canvas(canvas)
                        .style(StyleBuilder::new().size(mark_size, mark_size).build())
                        .layout_override(Rect::new(inset, inset, mark_size, mark_size))
                        .bind_style(move |cx| {
                            let _ = theme.appearance(cx);
                            StylePatch::default()
                        })
                        .a11y(A11yOverride {
                            hidden: true,
                            ..A11yOverride::default()
                        })
                        .build(
                            app.window_mut(window)
                                .ok_or(EngineError::UnknownWindow { window })?
                                .tree_mut(),
                            control,
                        )?;
                    (Some(mark), None)
                }
                ToggleKind::Radio => {
                    let dot = device_length_with_parity(side * RADIO_DOT_RATIO, scale, side_px);
                    let inset = (side - dot) / 2.0;
                    let theme_dot = theme;
                    let mark = NodeBuilder::boxed()
                        .style(StyleBuilder::new().size(dot, dot).build())
                        .layout_override(Rect::new(inset, inset, dot, dot))
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
                        )?;
                    (Some(mark), None)
                }
                ToggleKind::Switch => (None, None),
            };
            Ok(Control {
                node: control,
                knob: None,
                ring,
                mark,
                mark_paint,
            })
        }
        ToggleKind::Switch => {
            let metrics = track_metrics(size, window_scale(app, window));
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
            let knob_size = metrics.height - metrics.inset * 2.0;
            let initial = Rect::new(metrics.inset, metrics.inset, knob_size, knob_size);
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
                mark_paint: None,
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
        math::{Point, Scale},
        node::id::NodeId,
        paint::color::{Color, ColorResolver},
        test_support::app::TestApp,
        window::WindowOptions,
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
        harness_at_scale(1.0, kind, checked, configure)
    }

    fn harness_at_scale(
        scale: f32,
        kind: ToggleKind,
        checked: bool,
        configure: impl FnOnce(Toggle) -> Toggle,
    ) -> Harness {
        let mut app = TestApp::with_options(WindowOptions {
            scale: Scale::new(scale).expect("测试 DPI 必须是有限正数"),
            ..WindowOptions::default()
        });
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

    fn ring_node(harness: &Harness) -> NodeId {
        harness
            .toggle
            .inner
            .geometry
            .borrow()
            .ring
            .expect("单选外圈节点")
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
                style.inner.padding.left,
                taffy::style::LengthPercentage::length(size.px()),
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
    fn the_switch_knob_is_vertically_centered_on_every_device_scale() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let harness = harness_at_scale(scale, ToggleKind::Switch, true, std::convert::identity);
            let track = harness
                .app
                .probe()
                .bounds(harness.toggle.control_node().expect("track"))
                .expect("track bounds");
            let knob = harness
                .app
                .probe()
                .bounds(harness.toggle.knob_node().expect("knob"))
                .expect("knob bounds");
            let top = ((knob.origin.y - track.origin.y) * scale).round();
            let bottom = (((track.origin.y + track.size.height)
                - (knob.origin.y + knob.size.height))
                * scale)
                .round();
            assert_eq!(top, bottom, "scale {scale}: 滑块上下边距必须相等");
            assert_eq!(
                (knob.size.width * scale).round(),
                (knob.size.height * scale).round(),
                "scale {scale}: 滑块必须是物理像素正方形"
            );
        }
    }

    #[test]
    fn the_mark_is_square_and_centered_on_every_device_scale() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            for kind in [ToggleKind::Checkbox, ToggleKind::Radio] {
                let harness = harness_at_scale(scale, kind, true, std::convert::identity);
                let control = harness
                    .app
                    .probe()
                    .bounds(harness.toggle.control_node().expect("control"))
                    .expect("control bounds");
                let container = if kind == ToggleKind::Radio {
                    harness
                        .app
                        .probe()
                        .bounds(ring_node(&harness))
                        .expect("ring bounds")
                } else {
                    control
                };
                if kind == ToggleKind::Radio {
                    assert_eq!(
                        (container.size.width * scale).round(),
                        (container.size.height * scale).round(),
                        "scale {scale}: 单选外圈必须是物理像素正圆"
                    );
                }
                let mark = harness
                    .app
                    .probe()
                    .bounds(mark_node(&harness))
                    .expect("mark bounds");
                let left = ((mark.origin.x - container.origin.x) * scale).round();
                let right = (((container.origin.x + container.size.width)
                    - (mark.origin.x + mark.size.width))
                    * scale)
                    .round();
                let top = ((mark.origin.y - container.origin.y) * scale).round();
                let bottom = (((container.origin.y + container.size.height)
                    - (mark.origin.y + mark.size.height))
                    * scale)
                    .round();
                assert_eq!(left, right, "scale {scale} {kind:?}: 标记左右边距必须相等");
                assert_eq!(top, bottom, "scale {scale} {kind:?}: 标记上下边距必须相等");
                assert_eq!(
                    (mark.size.width * scale).round(),
                    (mark.size.height * scale).round(),
                    "scale {scale} {kind:?}: 标记必须是物理像素正方形"
                );
            }
        }
    }

    #[test]
    fn the_radio_ring_stays_circular_after_a_device_scale_change() {
        let mut harness = harness(ToggleKind::Radio, true);
        let window = harness.app.window_id();
        harness
            .app
            .app_mut()
            .window_mut(window)
            .expect("window")
            .set_scale(Scale::new(1.25).expect("scale"));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.toggle.sync(harness.app.app_mut());
        let _ = harness.app.drain().expect("frame");

        let control = harness
            .app
            .probe()
            .bounds(harness.toggle.control_node().expect("control"))
            .expect("control bounds");
        let ring = harness
            .app
            .probe()
            .bounds(ring_node(&harness))
            .expect("ring bounds");
        let scale = 1.25;
        assert_eq!(
            (ring.size.width * scale).round(),
            (ring.size.height * scale).round(),
            "单选外圈必须是物理像素正圆"
        );
        let left = ((ring.origin.x - control.origin.x) * scale).round();
        let right = (((control.origin.x + control.size.width) - (ring.origin.x + ring.size.width))
            * scale)
            .round();
        let top = ((ring.origin.y - control.origin.y) * scale).round();
        let bottom = (((control.origin.y + control.size.height)
            - (ring.origin.y + ring.size.height))
            * scale)
            .round();
        assert_eq!(left, right, "单选外圈左右边距必须相等");
        assert_eq!(top, bottom, "单选外圈上下边距必须相等");
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
